package main

import (
	"context"
	"crypto/rand"
	"crypto/sha1"
	"encoding/hex"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"io/fs"
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
	"github.com/Kheopsian/hydra/internal/engine/grpcclient"
	"github.com/Kheopsian/hydra/internal/engine/ltclient"
	"github.com/Kheopsian/hydra/internal/fsinfo"
	"github.com/Kheopsian/hydra/internal/health"
	"github.com/Kheopsian/hydra/internal/jobs"
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
// A unix socket is a kernel object, not a file a share can host, so a data_dir
// on CIFS/NFS cannot hold one at all — bind fails and the engine never starts.
// The sockets are runtime state anyway, not data: put them in a local runtime
// directory instead, keyed by data_dir so two instances never collide.
func engineSocketPath(dataDir, name string, tcpPort int) string {
	if defaultEngineTCP || os.Getenv("HYDRA_ENGINE_TCP") != "" {
		return fmt.Sprintf("tcp://127.0.0.1:%d", tcpPort)
	}
	if net, _ := fsinfo.IsNetwork(dataDir); net {
		sum := sha1.Sum([]byte(dataDir))
		dir := filepath.Join(os.TempDir(), "hydra-"+hex.EncodeToString(sum[:4]))
		if err := os.MkdirAll(dir, 0700); err == nil {
			return filepath.Join(dir, name+".sock")
		}
		// No local scratch either — TCP loopback is the last resort.
		return fmt.Sprintf("tcp://127.0.0.1:%d", tcpPort)
	}
	return filepath.Join(dataDir, name+".sock")
}

// writeDefaultConfig seeds a fresh config at target from the embedded template,
// creating the parent directory if needed. dataDir overrides the template's
// data_dir; empty means the portable relative "data" (resolved next to the
// executable at boot).
func writeDefaultConfig(target, dataDir string) error {
	if dataDir == "" {
		dataDir = "data"
	}
	doc := hydraroot.DefaultConfigTOML
	if d, err := config.SetTOMLValue(doc, "daemon", "data_dir", fmt.Sprintf("%q", dataDir)); err == nil {
		doc = d
	}
	if defaultAPIHost != "" {
		if d, err := config.SetTOMLValue(doc, "daemon", "api_host", `"`+defaultAPIHost+`"`); err == nil {
			doc = d
		}
	}
	dir := filepath.Dir(target)
	if dir == "" {
		dir = "."
	}
	if dir != "." {
		if err := os.MkdirAll(dir, 0755); err != nil {
			return err
		}
	}
	tmp, err := os.CreateTemp(dir, ".default.toml.*")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name()) // a no-op once the rename succeeded
	if _, err := tmp.WriteString(doc); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	// CreateTemp opens 0600; a config Hydra seeds is world-readable, like the
	// one entrypoint.sh copies in.
	if err := os.Chmod(tmp.Name(), 0644); err != nil {
		return err
	}
	if _, err := os.Stat(target); err == nil {
		return os.ErrExist
	}
	if err := os.Rename(tmp.Name(), target); err != nil {
		return err
	}
	chownSeeded(target, dir)
	return nil
}

// chownSeeded hands a freshly seeded config to the PUID/PGID account when we
// run as root. entrypoint.sh chowns the config directory before dropping
// privileges; a deployment that overrides the entrypoint gets no such pass and
// would leave a root-owned config in a volume the app is about to read as
// somebody else. Failures are ignored: this is a courtesy, not a precondition
// for starting.
func chownSeeded(target, dir string) {
	if os.Geteuid() != 0 {
		return
	}
	uid, err := strconv.Atoi(os.Getenv("PUID"))
	if err != nil {
		return
	}
	gid, err := strconv.Atoi(os.Getenv("PGID"))
	if err != nil {
		gid = uid
	}
	_ = os.Chown(target, uid, gid)
	if dir != "." {
		_ = os.Chown(dir, uid, gid)
	}
}

// resolveAgentToken picks the shared bearer token the HydraAgent gRPC
// data-plane requires, in decreasing order of precedence: --agent-token, then
// $HYDRA_AGENT_TOKEN, then [daemon] agent_token. The environment is read so an
// orchestrator can inject the secret the way it injects every other secret --
// a Kubernetes secretKeyRef, a systemd EnvironmentFile, a compose env_file --
// instead of putting it in a command line that shows up in `ps` and in the
// manifest itself. It returns the token and the name of the source it came
// from, for a log line that says where the token came from without printing
// it.
//
// An empty environment variable counts as absent, not as "no auth": an unset
// or not-yet-populated secret must fall through to the config rather than
// silently serve the data-plane unauthenticated. Turning auth off explicitly
// is what --agent-token="" is for, which is why the flag is tracked by
// presence and not by value.
func resolveAgentToken(flagVal string, flagSet bool, cfgVal string) (token, source string) {
	if flagSet {
		return strings.TrimSpace(flagVal), "--agent-token"
	}
	// Trimmed because a token handed over by an env file or a mounted secret
	// almost always arrives with a trailing newline, and a token that differs
	// from the front's by one invisible byte fails authentication with nothing
	// on either side to explain why.
	if v := strings.TrimSpace(os.Getenv(agentwire.TokenEnv)); v != "" {
		return v, "$" + agentwire.TokenEnv
	}
	if v := strings.TrimSpace(cfgVal); v != "" {
		return v, "[daemon] agent_token"
	}
	return "", ""
}

// resolveConfigPath picks the config file. With --config given explicitly we
// use that path, seeding a fresh config there when the file does not exist.
// Otherwise try, in order: the compiled default path, a default.toml next to
// the executable, then default.toml in the working dir. If none exist, write a
// fresh default.toml next to the executable (with a relative data_dir) so a
// freshly-unzipped, double-clicked install just runs. The second return value
// reports whether a config was seeded, so the caller can say so once the log
// mirror is attached (it lives beside the config we are still resolving).
// seed=false means "use this path if it exists, but never create it": an agent
// configured from its environment holds no config file, and writing one would
// put a full template back in the volume it was meant to do without.
func resolveConfigPath(def string, explicit, seed bool) (string, bool) {
	if !seed {
		return def, false
	}
	if explicit {
		st, err := os.Stat(def)
		switch {
		case err == nil && !st.IsDir():
			return def, false
		case err == nil:
			// A directory is a path mistake, not an empty volume. Let
			// config.Load fail with the real reason rather than seeding
			// something next to it.
			slog.Error("the requested config path is a directory, not a file", "path", def)
			return def, false
		case !errors.Is(err, fs.ErrNotExist):
			// Unreadable is not missing: a permission error here means the
			// file may well exist, and writing over it is the last thing we
			// want. Same treatment — leave it to config.Load to report.
			slog.Error("could not check the requested config path", "path", def, "err", err)
			return def, false
		}
		// A missing --config used to be fatal, which is exactly the wrong
		// behaviour under an orchestrator: k8s mounts an empty volume, the
		// container dies before it can write anything, and it dies again on
		// every restart. Seed it instead — the same first-run seeding
		// entrypoint.sh does, but for every mode (--agent-only included) and
		// for deployments that override the entrypoint. An absolute config
		// path also takes its own directory as data_dir, matching what
		// entrypoint.sh writes, so an empty /config volume becomes a complete
		// install rather than one scattering data next to the binary.
		dataDir := "data"
		if dir := filepath.Dir(def); filepath.IsAbs(dir) {
			dataDir = dir
		}
		switch err := writeDefaultConfig(def, dataDir); {
		case err == nil:
			return def, true
		case errors.Is(err, os.ErrExist):
			// Another process seeded the same path while we were writing.
			// Its copy is the one to read.
			return def, false
		default:
			slog.Error("no config at the requested path and could not write one", "path", def, "err", err)
			return def, false
		}
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
			return c, false
		}
	}
	// Nothing found — write a fresh default next to the executable.
	target := "default.toml"
	if exeDir != "" {
		target = filepath.Join(exeDir, "default.toml")
	}
	switch err := writeDefaultConfig(target, ""); {
	case err == nil:
		return target, true
	case errors.Is(err, os.ErrExist):
		return target, false
	default:
		slog.Warn("no config found and could not write a default; using compiled default path", "target", target, "err", err)
		return def, false
	}
}

// subcommands are the one-shot maintenance commands. Each must be the first
// argument: everything after it belongs to it, and the daemon never starts.
var subcommands = []string{"hash-password", "reset-password"}

// misplacedSubcommand returns a subcommand found anywhere but first, so we can
// say so instead of silently starting the daemon. `hydra --config x
// reset-password y` used to boot the server and sit there, which reads as a
// hang rather than as a mistake, and cost a user a long evening.
func misplacedSubcommand(args []string) string {
	for _, a := range args {
		for _, sub := range subcommands {
			if a == sub {
				return sub
			}
		}
	}
	return ""
}

// configPathFromArgs picks the config path out of a subcommand's arguments.
// The path is positional, but `--config <path>` and `--config=<path>` are
// accepted too: that is how the daemon takes it, so it is what everyone tries
// first. Passing it that way used to store the literal string "--config" as
// the filename and fail with "open --config: no such file or directory".
func configPathFromArgs(args []string) string {
	for i := 0; i < len(args); i++ {
		a := args[i]
		switch {
		case a == "--config" || a == "-config":
			if i+1 < len(args) {
				return args[i+1]
			}
		case strings.HasPrefix(a, "--config="):
			return strings.TrimPrefix(a, "--config=")
		case strings.HasPrefix(a, "-config="):
			return strings.TrimPrefix(a, "-config=")
		case !strings.HasPrefix(a, "-"):
			return a
		}
	}
	return ""
}

// leftoverArgsMessage explains a positional argument the daemon cannot take.
// flag.Parse stops at the first non-flag argument and drops everything after
// it into flag.Args() in silence, so `hydra --config x hydra --config x
// --front-only` parses --config, ignores --front-only and boots a monolith.
// The container entrypoint already runs `hydra --config <cfg> "$@"`, so a
// compose `command:` that repeats the binary name lands here: the shape flags
// are dropped and the node comes up as the wrong kind of node, with nothing in
// the log to say so. Returns "" when there is nothing to report.
func leftoverArgsMessage(args []string) string {
	if len(args) == 0 {
		return ""
	}
	msg := fmt.Sprintf("unexpected argument %q: hydra takes flags only\n", args[0])
	// Only claim flags were dropped when some actually were: a lone stray
	// positional drops nothing, and saying otherwise sends the reader hunting
	// for a flag that was never there.
	if len(args) > 1 {
		msg += fmt.Sprintf("everything after it was ignored: %s\n", strings.Join(args[1:], " "))
	}
	// Windows resolves the name case-insensitively, so `Hydra.exe` is the same
	// mistake and deserves the same hint. Cut on both separators by hand rather
	// than with filepath.Base: this string comes from the user's compose file,
	// not from the running filesystem, so a Windows path must be understood by
	// a Linux build too -- filepath.Base there does not treat `\` as one and
	// hands back `C:\hydra\Hydra.exe` whole.
	if base := strings.ToLower(args[0][strings.LastIndexAny(args[0], `/\`)+1:]); base == "hydra" || base == "hydra.exe" {
		msg += "the container entrypoint already runs `hydra --config <config>`, so a compose\n" +
			"`command:` must carry the EXTRA FLAGS ONLY, e.g.\n" +
			"  command: [\"--agent-only\", \"--agent-addr\", \":9090\"]\n"
	}
	return msg + "(refusing to start rather than run with the flags silently dropped)\n"
}

func main() {
	// A subcommand that is not first is a mistake, not a request to serve.
	if len(os.Args) > 2 {
		if sub := misplacedSubcommand(os.Args[2:]); sub != "" {
			fmt.Fprintf(os.Stderr,
				"%s must be the first argument: hydra %s ...\n"+
					"(seen after other arguments; refusing to start the daemon instead)\n",
				sub, sub)
			os.Exit(2)
		}
	}

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
			fmt.Fprintln(os.Stderr, "       hydra reset-password <newpassword> --config <config-path>")
			os.Exit(2)
		}
		cfgPath := configPathFromArgs(os.Args[3:])
		if cfgPath == "" {
			if d := os.Getenv("HYDRA_CONFIG_DIR"); d != "" {
				cfgPath = filepath.Join(d, "default.toml")
			} else {
				cfgPath = "/config/default.toml"
			}
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
	agentToken := flag.String("agent-token", "", "shared bearer token required by the HydraAgent gRPC API; overrides $"+agentwire.TokenEnv+" and [daemon] agent_token (empty everywhere = no auth)")
	agentTLSCert := flag.String("agent-tls-cert", "", "TLS cert file for the HydraAgent gRPC API (with --agent-tls-key)")
	agentTLSKey := flag.String("agent-tls-key", "", "TLS key file for the HydraAgent gRPC API")
	agentOnly := flag.Bool("agent-only", false, "run as a dedicated agent: engines + gRPC data-plane, no api.Server, owns events")
	healthAddr := flag.String("health-addr", "", "agent-only: serve GET /health on this addr for an orchestrator's container probe; empty = [daemon] api_host:api_port, \"off\" = no HTTP listener at all")
	var engineSpecs engineSpecFlag
	flag.Var(&engineSpecs, "engine", "agent-only: declare one engine this node hosts, as id=race-0,role=race,port=12314,ipv6=true. Repeatable. Overrides $"+envEngines+" / $"+envEngineID+" and the config file; everything else about the engine comes from the front")
	listenPortHook := flag.Int("listen-port-hook", 0, "agent-only: serve a loopback-only (127.0.0.1) HTTP POST /listen-port hook on this port so a co-netns gluetun UP_COMMAND can push the forwarded BT port; 0 = disabled (also $"+envListenPortHook+")")
	bootFromStore := flag.Bool("boot-from-store", true, "load torrents from the SQLite store (content-addressed, durable); state.json runs as an overlay/fallback. Default on since v2.9.x; disable with --boot-from-store=false")
	flag.Parse()
	// flag.Parse stops at the first positional argument; the daemon takes
	// none, so a leftover means the flags behind it were dropped on the floor.
	if msg := leftoverArgsMessage(flag.Args()); msg != "" {
		fmt.Fprint(os.Stderr, msg)
		os.Exit(2)
	}

	if *showVersion {
		fmt.Println("hydra", version)
		os.Exit(0)
	}

	// Structured logging funnels into the in-process hub (ring buffer for the
	// UI "Logs" tab + hydra.log mirror). The console stays clean: only ERROR
	// surfaces there, plus the explicit human startup banner below.
	logHub := logs.Default
	// $HYDRA_LOG_STDOUT sends the mirror to stdout instead of the file. It
	// needs no config path, so attach it here rather than next to the
	// SetMirrorFileBeside call below: the mirror then catches every line of
	// the run, including the ones logged while the config is being resolved.
	stdoutLogs := logs.StdoutMirror()
	if stdoutLogs {
		logHub.SetMirrorStdout()
	}
	handlers := []slog.Handler{logs.NewSlogHandler(logHub, slog.LevelInfo)}
	if !stdoutLogs {
		// With the mirror on stdout every ERROR is already on the console;
		// the stderr handler would print each one a second time.
		handlers = append(handlers, slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelError}))
	}
	logger := slog.New(logs.NewMultiHandler(handlers...))
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
	configExplicit, agentTokenExplicit := false, false
	flag.Visit(func(f *flag.Flag) {
		switch f.Name {
		case "config":
			configExplicit = true
		case "agent-token":
			// Recorded rather than inferred from an empty value: passing
			// --agent-token="" is how you turn auth off on a node whose
			// config or environment sets a token.
			agentTokenExplicit = true
		}
	})
	// An agent whose identity comes from flags or the environment needs no
	// config file at all: it takes everything else from its front. Seeding one
	// would write a full template into its volume and re-create exactly the
	// duplication this mode exists to remove.
	fileless := *agentOnly && agentIdentityFromArgs(engineSpecs)
	resolved, seeded := resolveConfigPath(*configPath, configExplicit, !fileless)
	*configPath = resolved
	if !stdoutLogs {
		logHub.SetMirrorFileBeside(*configPath, "hydra.log")
	}
	if seeded {
		if configExplicit {
			// Worth a warning, not a note: a typo in --config lands here too,
			// and a fresh config means an instance that knows nothing about
			// the data the operator expected it to pick up.
			slog.Warn("no config file at the requested path, wrote a fresh default there", "path", *configPath)
		} else {
			slog.Info("no config file found, wrote a fresh default", "path", *configPath)
		}
	}

	cfg, err := config.Load(*configPath)
	switch {
	case err == nil:
	case fileless && errors.Is(err, fs.ErrNotExist):
		// Expected: this agent was handed its identity on the command line or
		// in its environment and takes everything else from its front.
		cfg = config.DefaultConfig()
		slog.Info("no config file, running on the environment plus what the front pushes", "path", *configPath)
	default:
		slog.Error("Failed to load config", "path", *configPath, "error", err)
		os.Exit(1)
	}
	if v := strings.TrimSpace(os.Getenv(envDataDir)); v != "" {
		cfg.Daemon.DataDir = v
	}
	// Cle API auto-generee au 1er demarrage UNIQUEMENT si vide (une install fraiche
	// = api_key="" dans le template) et persistee -> cle unique sans config manuelle.
	// On ne touche PAS une cle existante meme "change-me-in-production" : ecraser une
	// cle en place casse les clients (autobrr/arr) qui l'utilisent deja.
	// Skipped without a config file: an agent-only node serves no HTTP API, so
	// the key would authenticate nothing and could not be persisted anywhere.
	if cfg.Daemon.APIKey == "" && !fileless {
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
	// The data-plane token is resolved once, here, so both the monolith's
	// optional --agent-addr listener and the dedicated --agent-only node get
	// the same value from the same three sources.
	agentSecret, agentSecretFrom := resolveAgentToken(*agentToken, agentTokenExplicit, cfg.Daemon.AgentToken)
	if agentSecret != "" {
		slog.Info("HydraAgent token loaded", "from", agentSecretFrom)
	}
	// No admin password is generated here on purpose. Hydra used to mint one at
	// this point and only report it ~700 lines later, after the engines were up
	// — so any boot that failed in between (a data_dir the engines could not
	// use, most often) left password_hash set in the config and the plaintext
	// nowhere: the account was consumed and the install unreachable. An empty
	// password_hash now means "first run", and the UI asks a human to pick one.
	// Nothing is generated, so nothing can be lost.
	firstRun := cfg.Auth.PasswordHash == ""
	// An agent-only node serves no web UI, so pointing its operator at one is
	// advice they cannot follow. Its access control is the agent token.
	if firstRun && !*agentOnly {
		slog.Info("first run: no admin password set, open the web UI to create one")
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

	// data_dir on a share is a supported-but-degraded setup, not a crash: the
	// store drops to a rollback journal under an exclusive lock and the engine
	// sockets move to local scratch. Say so once, loudly — the risk is real and
	// it is the user's to accept. Torrent data on a share needs none of this:
	// the engine only ever does positional reads and writes there.
	if net, kind := fsinfo.IsNetwork(cfg.Daemon.DataDir); net {
		api.SetStorageWarning(string(kind))
		slog.Warn("data_dir is on network storage: running in degraded mode",
			"path", cfg.Daemon.DataDir, "filesystem", string(kind))
		slog.Warn("  the database cannot use WAL here; a dropped share mid-write can corrupt it. KEEP BACKUPS")
		slog.Warn("  for full speed and safety, point data_dir at local disk; your downloads can stay on the share")
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
	// Only the monolith needs both roles: it drives raceCfg and hoardCfg
	// directly below and would nil-deref without them. A front-only or
	// agent-only node runs whatever set ResolveEngines gave it, so enforcing
	// the pair here would reject a dedicated agent hosting a single engine --
	// which is the whole point of an agent node.
	if *agentOnly {
		// Not engineCfgs: an agent takes its identity from flags or the
		// environment as often as from the file, and resolveAgentBoot logs the
		// engines it actually resolved. Repeating a file-derived count here
		// would contradict it.
		slog.Info("Config loaded", "data_dir", cfg.Daemon.DataDir)
	} else if *frontOnly {
		slog.Info("Config loaded",
			"engines", len(engineCfgs),
			"data_dir", cfg.Daemon.DataDir,
		)
	} else {
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
	}

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
	var storeRepair *api.RepairState
	var storeReady atomic.Bool
	// The second half of the same gate. The API server is already serving by the
	// time the boot-from-store import runs, and a saveState it triggers syncs the
	// engine's view over the store wholesale. Until the import has handed the
	// engines what the store knows, that view is the resume data's -- no
	// category, no save path -- and syncing it blanks the rows the category
	// backfill has just repaired. Nothing syncs until the import is done.
	var storeImported atomic.Bool
	if *frontOnly || *agentOnly {
		// controller / agent mode: no monolith shadow store (agents own their
		// per-agent DBs; a shadow hydra.db here would be a parasitic dangling file).
	} else if storeRepair = storeRepairDiagnosis(cfg.Daemon.DataDir); storeRepair.Needed {
		// The databases cannot be opened from where data_dir now points. Open
		// nothing, migrate no sidecars, start no engine: every path below here
		// assumes a store, and the one that carries on without one rewrites the
		// JSON carry-overs from an empty memory. See storerepair.go.
		api.SetRepairState(storeRepair)
		logStoreRepairNeeded(storeRepair)
	} else if ts, terr := store.Open(filepath.Join(cfg.Daemon.DataDir, "hydra.db")); terr != nil {
		slog.Error("store: open failed, shadow persistence disabled", "error", terr)
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
				slog.Error("store: sidecar import had errors, originals kept in place",
					"report", rep.String(), "errors", rep.Errors)
			} else {
				slog.Info("store: imported JSON sidecars (originals renamed .migrated)", "report", rep.String())
			}
			if len(rep.Superseded) > 0 {
				// These came back after the store already held their values, so
				// something wrote them without a store to write to. Say it out
				// loud: on the installs this guard was written for, importing
				// them is what erased the lifetime counters.
				slog.Warn("store: ignored JSON sidecars that reappeared after the store already held their values",
					"files", fmt.Sprint(rep.Superseded))
				slog.Warn("  they were written by a boot that could not open the store, and are kept as .superseded")
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
			// Re-seed the seed clock from the durable copy. Has to happen
			// before storeReady opens the sync gate: a sync that ran first
			// would write this process's fresh zeros over the history with
			// nothing to MAX() against in memory.
			if saved, serr := torStore.AllSeedTimes(); serr != nil {
				slog.Warn("store: cannot load seed times, counters restart from zero", "error", serr)
			} else if len(saved) > 0 {
				engine.SeedTimeSeed(saved)
				slog.Info("store: seed time counters restored", "torrents", len(saved))
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
		boot, identitySource, berr := resolveAgentBoot(engineSpecs, cfg)
		if berr != nil {
			slog.Error("agent-only: could not resolve the engine identity", "source", identitySource, "error", berr)
			os.Exit(1)
		}
		runAgentOnly(ctx, cfg, boot, identitySource,
			envOr(*agentAddr, envAgentAddr), agentSecret,
			envOr(*agentTLSCert, envAgentTLSCert), envOr(*agentTLSKey, envAgentTLSKey),
			envIntOr(*listenPortHook, envListenPortHook), envOr(*healthAddr, envHealthAddr))
		return
	}
	if storeRepair != nil && storeRepair.Needed {
		runStoreRepair(ctx, cfg)
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

	api.InitAnnounceOverrides(cfg)
	raceEngine := engine.NewRaceEngine(raceCfg, chokingEngine, nil, raceDataDir)
	raceEngine.SetClient(raceProc.Client())

	hoardEngine := engine.NewHoardEngine(hoardCfg, hoardDataDir)

	// Pins live in the durable store rather than beside the engine: the engine
	// keeps them in memory (IsPinned is read on every slot tick) and writes
	// through here. Nothing is carried over from the old hoard_pinned.json --
	// a pin only claims a download slot, so a pin on a torrent that is no
	// longer incomplete says nothing worth keeping.
	if torStore != nil {
		if pins, err := torStore.PinnedList(); err != nil {
			slog.Warn("hoard: pin load failed", "error", err)
		} else {
			hoardEngine.SetPins(pins)
		}
		hoardEngine.SetPinPersister(func(infoHash string, pinned bool) {
			var err error
			if pinned {
				err = torStore.SetPinned(infoHash, true)
			} else {
				err = torStore.SetPinned(infoHash, false)
			}
			if err != nil {
				slog.Warn("hoard: pin persist failed", "info_hash", infoHash, "pinned", pinned, "error", err)
			}
		})
	}
	hoardEngine.CreateTorrentFolder = cfg.Daemon.CreateTorrentFolder
	hoardEngine.SetClient(hoardProc.Client())

	// ---- Optional: HydraAgent gRPC data-plane (pivot 25/07) ----
	// Additive: only when --agent-addr is set. Exposes this machine's engine
	// sessions as EngineClient mirrors so a remote front can drive them. Off by
	// default = zero impact on the monolith. ownEvents stays false: the
	// in-process engines already own SetEventHandler on these live clients.
	if *agentAddr != "" {
		if agentSecret == "" {
			slog.Warn("HydraAgent served WITHOUT a token: set --agent-token, $" + agentwire.TokenEnv + " or [daemon] agent_token before exposing it off-LAN")
		}
		agentSrv := agent.NewServer(map[string]engine.EngineClient{
			agentwire.EngineRace:  raceProc.Client(),
			agentwire.EngineHoard: hoardProc.Client(),
		}, cfg.Daemon.DataDir, agentSecret)
		agentSrv.SetTLS(*agentTLSCert, *agentTLSKey)
		agentSrv.SetRichEngines(raceEngine, hoardEngine)
		agentSrv.SetIPv6Wanted(raceCfg.EnableIPv6 || hoardCfg.EnableIPv6)
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
	// Binds the forwarded port, releases the hold placed above, then follows
	// the port for as long as the daemon runs.
	apiServer.StartGluetunPortSync(map[string]*config.SessionConfig{"race": raceCfg, "hoard": hoardCfg})
	apiServer.SetStateManager(stateMgr)

	// ---- Background jobs ----
	//
	// Payload moves take minutes to hours, so they run here rather than
	// inside a request. The manager is also what a rules engine will submit
	// to later; a move is simply the first registered job type.
	//
	// Concurrency is one on purpose: a move is sequential I/O against the
	// same disks the torrents are served from, so running several at once
	// makes every one of them slower and the seeding worse.
	// Set when a job manager exists; called once agents are registered.
	var resumeJobs func()
	if torStore != nil {
		jobMgr := jobs.NewManager(ctx, torStore, 1)
		jobMgr.Register(&jobs.MoveRunner{
			Host: func(infoHash string) jobs.PayloadHost {
				if hoardEngine != nil && hoardEngine.HasTorrent(infoHash) {
					return hoardEngine
				}
				if raceEngine != nil && raceEngine.HasTorrent(infoHash) {
					return raceEngine
				}
				return nil
			},
		})
		// Cross-node moves. The dialers resolve at RUN time, not here: agents
		// are registered further down in boot, and a resumed job must find the
		// agent as it is now rather than as it was when the job was created.
		// An empty agent name, or "local", means this node. Resolving it to a
		// localEndpoint is what lets a monolith be one end of a move -- the
		// common case, since the front is usually where the library lives.
		isLocal := func(agent string) bool { return agent == "" || agent == api.LocalAgentName }
		localFor := func(engineID, category string) *localEndpoint {
			client := hoardProc.Client()
			if engineID == agentwire.EngineRace {
				client = raceProc.Client()
			}
			le := &localEndpoint{client: client, dataDir: cfg.Daemon.DataDir}
			// Only the hoard has an adopt path that registers Go-side metadata;
			// a race target keeps the plain engine import.
			if engineID != agentwire.EngineRace && hoardEngine != nil {
				le.adopt = func(rec *ltclient.ResumeRecord) error {
					return hoardEngine.AdoptTorrent(rec, category)
				}
			}
			return le
		}
		jobMgr.Register(&jobs.RemoteMoveRunner{
			DialSource: func(p jobs.RemoteMoveParams) (jobs.PieceSource, error) {
				if isLocal(p.SourceAgent) {
					return localFor(p.Engine, p.Category), nil
				}
				return apiServer.RemoteAgentEngineClient(p.SourceAgent, p.Engine)
			},
			DialSink: func(p jobs.RemoteMoveParams) (jobs.PieceSink, error) {
				if isLocal(p.TargetAgent) {
					return localFor(p.Engine, p.Category), nil
				}
				return apiServer.RemoteAgentEngineClient(p.TargetAgent, p.Engine)
			},
			ResolveSavePath: apiServer.CategorySavePathFor,
			SetTargetCategory: func(p jobs.RemoteMoveParams, infoHash string) error {
				if isLocal(p.TargetAgent) {
					if hoardEngine == nil {
						return nil
					}
					return hoardEngine.SetCategoryLabel(infoHash, p.Category)
				}
				cl, err := apiServer.RemoteAgentEngineClient(p.TargetAgent, p.Engine)
				if err != nil {
					return err
				}
				return cl.SetCategoryLabel(p.Engine, infoHash, p.Category)
			},
			FreeSpace: func(agent, path string) (int64, error) {
				if isLocal(agent) {
					return localFreeSpace(path)
				}
				cl, err := apiServer.RemoteAgentEngineClient(agent, agentwire.EngineHoard)
				if err != nil {
					return 0, err
				}
				return cl.DiskFree(path)
			},
		})
		apiServer.SetJobManager(jobMgr)
		// Finished jobs are kept long enough to be useful and not forever.
		jobMgr.Prune(30 * 24 * time.Hour)
		resumeJobs = jobMgr.ResumeAll
	}
	// ---- Remote agents ([[agent]] + agents.json) for multi-home placement ----
	// The reconciler dials them, pushes each one its composed config, and keeps
	// doing both on a timer, so an agent that was down at boot or came back on a
	// stale config is picked up. A dead agent is logged + skipped, never blocks
	// boot.
	apiServer.StartAgentReconciler(ctx)

	// Only now can interrupted work be picked up. A cross-node job resolves its
	// agents when it runs, and resuming before they were registered failed it
	// outright with "no agent named ...": the very restart the job was built to
	// survive was what killed it. Still before the API takes new work, so a
	// resumed move never races a fresh one for the same torrent.
	if resumeJobs != nil {
		resumeJobs()
	}
	apiServer.SetRaceDrain(raceDrain)
	apiServer.SetGraduationReporter(gradr)
	apiServer.SetArrCleanup(arrCleanup)
	apiServer.SetBenchDB(&benchAPIAdapter{db: benchDB})
	apiServer.SetSaveStateCallback(func() {
		saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady, &storeImported)
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
	hoardAnnounceBindings := engine.ApplyAnnounceEgress(
		engine.DefaultSingleBinding(hoardCfg.ListenPort, hoardCfg.EnableIPv6, "hoard", hoardCfg.AnnounceRateLimit),
		hoardCfg.AnnounceProxy, hoardCfg.AnnounceIP, hoardCfg.Socks5OutboundHost, hoardCfg.BindInterface, "hoard")
	raceAnnounceBindings := engine.ApplyAnnounceEgress(
		engine.DefaultSingleBinding(raceCfg.ListenPort, raceCfg.EnableIPv6, "race", raceCfg.AnnounceRateLimit),
		raceCfg.AnnounceProxy, raceCfg.AnnounceIP, raceCfg.Socks5OutboundHost, raceCfg.BindInterface, "race")

	// Sessions following gluetun's forwarded port hold their announces from
	// here: the port is not known yet, and announcing the configured one would
	// hand every tracker an address that answers nobody for a full cycle.
	api.HoldForGluetun(map[string]*config.SessionConfig{"race": raceCfg, "hoard": hoardCfg})

	// Startup pause, armed before any announcer starts so nothing escapes in
	// the gap. Both halves matter: holding the gate stops announces leaving
	// from here, and set_dials_paused stops the engine dialing peers it
	// already knows (resume data, PEX, DHT). Either alone still lets a wave
	// of outbound flows through.
	for _, sp := range []struct {
		scope  string
		paused bool
		client engine.EngineClient
	}{
		{"hoard", hoardCfg.StartPaused, hoardProc.Client()},
		{"race", raceCfg.StartPaused, raceProc.Client()},
	} {
		if !sp.paused {
			continue
		}
		engine.HoldStartupPause(sp.scope)
		if err := engine.SetEngineDialsPaused(sp.client, true); err != nil {
			// The announce half is already held, so this is a partial hold,
			// not an open door -- say which half failed rather than implying
			// the pause is off entirely.
			slog.Error("startup pause: engine refused to hold its dial queue; announces are still held",
				"engine", sp.scope, "error", err)
		}
	}
	// Releasing is the mirror image, and lives here because this is where the
	// engine clients are. Dials are resumed before the announce gate lifts:
	// the other order would let an announce return peers the engine is still
	// refusing to dial, and they would be dropped rather than queued.
	apiServer.SetStartupPauseRelease(func() []string {
		for scope, client := range map[string]engine.EngineClient{
			"hoard": hoardProc.Client(), "race": raceProc.Client(),
		} {
			if err := engine.SetEngineDialsPaused(client, false); err != nil {
				slog.Error("startup pause: engine refused to resume its dial queue",
					"engine", scope, "error", err)
			}
		}
		released := []string{}
		for _, scope := range []string{"hoard", "race"} {
			if engine.ReleaseStartupPause(scope) {
				released = append(released, scope)
			}
		}
		return released
	})

	// With IPv6 off, announces are pinned to IPv4. On a host that has no IPv4
	// that pin has nowhere to go, and the honest place to say so is here, once,
	// rather than in every announce error that follows.
	if (!hoardCfg.EnableIPv6 || !raceCfg.EnableIPv6) && !engine.HostHasIPv4() {
		slog.Warn("announces are pinned to IPv4 because enable_ipv6 is off, but this host has no IPv4 address")
		slog.Warn("  set enable_ipv6 = true under [race] and [hoard] if this host is IPv6-only")
	}
	hoardAnnouncer := engine.NewHoardAnnouncer(hoardProc.Client(), hoardAnnounceBindings)
	hoardAnnouncer.OnObservation = hoardEngine.ObserveAnnounce
	hoardAnnouncer.SetLivePort(hoardEngine.LivePort())
	// On-add bootstrap announce: a freshly-added download announces once
	// immediately so its swarm_seeds reaches the slot-manager cache before the
	// first slot decision (else seeds=0 -> parked -> never announced catch-22).
	hoardEngine.SetBootstrapAnnounce(hoardAnnouncer.BootstrapAnnounce)
	hoardEngine.SetReAnnounce(hoardAnnouncer.ReAnnounce)
	hoardEngine.SetStoppedAnnounce(hoardAnnouncer.StoppedAnnounce)
	hoardAnnouncer.SetUserStoppedGate(hoardEngine.IsUserPaused)
	// Anti dual-annonce : le hoard n'annonce PAS un infohash que le race tient
	// (le race est seul annonceur tant qu'il l'a) + offset de continuité au
	// handoff race->hoard. Le race lui-même n'est pas gaté (toujours annonceur).
	// [anti-dual removed 2026-08-01] no race-gate / no announce-offset:
	// race and hoard both announce + seed the same infohash (legit multi-seed).
	hoardAnnouncer.Start(ctx)
	raceAnnouncer := engine.NewHoardAnnouncer(raceProc.Client(), raceAnnounceBindings)
	raceAnnouncer.OnObservation = raceEngine.ObserveAnnounce
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
		// KNOWN ISSUE: this still captures too early. Measured 2026-08-12, the
		// offset lands at 0 even though the torrents are already loaded — the
		// engines' cached stats have not been refreshed yet, so GetSessionTotals()
		// has nothing to report. Seconds later those lifetime totals appear and
		// get booked as traffic from this session. Gating on the store import is
		// NOT the fix (tried: capture already happens after it). The offset has
		// to wait for the stats cache to reflect the loaded set. Telemetry only
		// — announces read the engine's per-torrent counters, not this.
		apiServer.CaptureSessionOffset()
	}()

	// ---- boot-from-store (SQLite, content-addressed) ----
	// Reversible read-path flip: load durable identity/metainfo from the store
	// instead of trusting state.json + uploads/ (whose reused filenames caused
	// silent torrent loss). The state.json block below still runs as a metadata
	// overlay / gap-fill. Flag defaults off -> monolith behaviour unchanged.
	if *bootFromStore && torStore != nil {
		// Repair before the import, not after: the engines take their category
		// from the store row as they load, so a backfill that ran later would
		// need a second restart to show up.
		backfillCategories(cfg.Daemon.DataDir, savedState, torStore)

		up := filepath.Join(cfg.Daemon.DataDir, "uploads")
		rN, rE := raceEngine.ImportFromStoreSession(torStore, store.Race, up)
		hN, hE := hoardEngine.ImportFromStoreSession(torStore, store.Hoard, up)
		slog.Info("boot-from-store: loaded torrents from SQLite store",
			"race", rN, "race_errors", rE, "hoard", hN, "hoard_errors", hE)
		storeImported.Store(true)

		// The content layout flag was the last thing only state.json knew: hand
		// it over once, then retire the file. From the next boot on the store is
		// the only source, and nothing rewrites tens of megabytes of JSON on
		// every save.
		if savedState != nil {
			moved := 0
			for ih, meta := range savedState.HoardActive {
				if meta.ContentFolder == nil {
					continue
				}
				if err := torStore.SetContentFolder(ih, meta.ContentFolder); err == nil {
					moved++
				}
			}
			unknown, _ := torStore.ContentFolderUnknown()
			statePath := filepath.Join(cfg.Daemon.DataDir, "state.json")
			renamed := os.Rename(statePath, statePath+".migrated") == nil
			slog.Info("state.json retired, the store is now the only source",
				"content_folder_moved", moved, "still_unknown", unknown, "renamed", renamed)
		}
	} else {
		// No import to wait on: the engines are as loaded as they are going to get.
		storeImported.Store(true)
	}

	// (the state.json metadata overlay lived here — the store carries all of
	// it now, including the content layout flag)
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
	// Detect our v6 the same way as our v4, but only if an engine actually
	// listens on IPv6: otherwise the address is of no use to the filter and the
	// lookup would be pure cost. One flag for both engines is enough — the
	// filter only ever drops dials to ourselves.
	selfIPsIncludeV6 := hoardCfg.EnableIPv6 || raceCfg.EnableIPv6
	go func() {
		push := func() {
			ips := api.PublicIPs(selfIPsIncludeV6)
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

	// Bench recording cadence and retention are configurable: race_snapshots
	// gets one row per racing torrent per tick, so both the interval and the
	// window decide how big bench.db gets.
	benchCfg := cfg.Bench
	benchDB.SetRetention(bench.RetentionPolicy{
		GeneralDays:      benchCfg.RetentionDays,
		RaceSnapshotDays: benchCfg.RaceSnapshotRetentionDays,
		Vacuum:           benchCfg.Vacuum,
	})
	benchSnapshotInterval := 5 * time.Second
	if benchCfg.SnapshotIntervalSecs > 0 {
		benchSnapshotInterval = time.Duration(benchCfg.SnapshotIntervalSecs) * time.Second
	}
	benchPruneInterval := time.Hour
	if benchCfg.PruneIntervalMins > 0 {
		benchPruneInterval = time.Duration(benchCfg.PruneIntervalMins) * time.Minute
	}
	benchRecording := benchCfg.BenchEnabled()
	if !benchRecording {
		slog.Info("bench: recording disabled by config ([bench] enabled=false); retention still runs")
	}

	go func() {
		if !benchRecording {
			return
		}
		ticker := time.NewTicker(benchSnapshotInterval)
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
		ticker := time.NewTicker(benchPruneInterval)
		defer ticker.Stop()
		// Prune once at boot too: an instance that has been running on the old
		// year-long window carries millions of stale race_snapshots, and
		// waiting a full interval to start on them serves nothing.
		benchDB.PurgeOld()
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
				saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady, &storeImported)
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
	slog.Info("  All systems GO (Typhon engine)")
	slog.Info("============================================================")

	logs.PrintReady(cfg.Daemon.APIHost, cfg.Daemon.APIPort, firstRun)

	// Windows: the notification-area icon is the app's only visible presence
	// and its only clean stop (no console => no Ctrl+C, and SIGTERM is never
	// delivered). Elsewhere this is a no-op returning a nil channel, which the
	// select below then ignores.
	trayQuit, trayStop := startTray(
		fmt.Sprintf("http://%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort),
		version,
		func() (up, down float64, torrents int) {
			if raceEngine != nil {
				st := raceEngine.GetAllStatus()
				up += toFloat(st["upload_rate"])
				down += toFloat(st["download_rate"])
				torrents += int(toFloat(st["torrents"]))
			}
			if hoardEngine != nil {
				st := hoardEngine.GetAllStatus()
				up += toFloat(st["upload_rate"])
				down += toFloat(st["download_rate"])
				torrents += int(toFloat(st["torrents"]))
			}
			return up, down, torrents
		},
	)
	defer trayStop()

	_ = notifier
	_ = metricsCollector
	_ = system.Collect

	select {
	case sig := <-sigCh:
		slog.Info("Received signal, shutting down", "signal", sig)
	case <-trayQuit:
		slog.Info("Quit from the notification-area menu, shutting down")
	}
	cancel()

	slog.Info("Saving state...")
	saveState(stateMgr, raceEngine, hoardEngine, torStore, &storeReady, &storeImported)

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

// backfillCategories repairs the categories the v3.50.0 store migration
// dropped, once per database.
//
// That release retired state.json but handed the store only the content-layout
// flag. The category column had never been filled for torrents added before the
// move -- the boot-time overlay removed in the same commit had been re-applying
// it from JSON every start -- so those torrents came up uncategorised.
//
// The source is whichever copy of the state the installation still has: the
// live state.json on an upgrade that has not been retired yet, and the
// state.json.migrated left behind on one that has. With neither there is
// nothing to repair and nothing is recorded, so an installation whose file is
// briefly unreadable gets another chance on the next boot.
func backfillCategories(dataDir string, savedState *state.State, torStore *store.Store) {
	cats := savedState.Categories()
	source := "state.json"
	if len(cats) == 0 {
		source = "state.json.migrated"
		retired := filepath.Join(dataDir, "state.json.migrated")
		loaded, err := state.CategoriesFrom(retired)
		if err != nil {
			if !os.IsNotExist(err) {
				slog.Warn("category backfill: cannot read retired state, will retry next boot",
					"path", retired, "error", err)
			}
			return
		}
		cats = loaded
	}
	if len(cats) == 0 {
		return
	}

	updated, ran, err := torStore.BackfillCategories(cats)
	switch {
	case err != nil:
		slog.Error("category backfill failed", "error", err)
	case !ran:
		// Already repaired: the user owns their categories from here on.
	case updated > 0:
		slog.Info("category backfill: restored categories dropped by the v3.50.0 store migration",
			"torrents", updated, "source", source)
	default:
		slog.Info("category backfill: nothing to repair", "source", source)
	}
}

// ---------------------------------------------------------------------------
// State persistence
// ---------------------------------------------------------------------------

func saveState(stateMgr *state.Manager, race *engine.RaceEngine, hoard *engine.HoardEngine, torStore *store.Store, ready, imported *atomic.Bool) {
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

	// state.json is not written any more: the store holds all of it.

	// Shadow-sync the SQLite store (best-effort; never affects state.json).
	// Both gates: the store has to be past its initial backfill, and the engines
	// past the boot import that hands them what the store knows. Syncing before
	// either one writes an incomplete view over a complete row.
	if torStore != nil && ready != nil && ready.Load() && imported != nil && imported.Load() {
		syncStore(torStore, raceMetas, hoardMetas, race.AllTotals(), hoard.AllTotals())
	}
}

func syncStore(st *store.Store, raceMetas, hoardMetas map[string]*engine.TorrentMeta, raceTotals, hoardTotals map[string][2]int64) {
	// One snapshot for the whole batch: the clock is a single map behind one
	// mutex, and a per-torrent lookup here would take it 200k times a tick.
	seedTimes := engine.SeedTimeAll()
	items := make([]store.SyncItem, 0, len(raceMetas)+len(hoardMetas))
	add := func(sess store.Session, metas map[string]*engine.TorrentMeta, totals map[string][2]int64) {
		for ih, m := range metas {
			t := totals[ih] // zero value when the engine has not reported it yet; MAX() in the store keeps history
			items = append(items, store.SyncItem{
				InfoHash:        ih,
				TotalUploaded:   t[0],
				TotalDownloaded: t[1],
				SeedingTime:     seedTimes[ih],
				Session:         sess,
				Paused:          m.UserPaused,
				Tags:            m.Tags,
				SavePath:        m.SavePath,
				Category:        m.Category,
				TorrentFilePath: m.TorrentFilePath,
				CompletedTime:   float64(timeToUnix(m.CompletedTime)),
				ContentFolder:   m.ContentFolder,
			})
		}
	}
	add(store.Race, raceMetas, raceTotals)
	add(store.Hoard, hoardMetas, hoardTotals)
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

// announceTick is the previous read of one engine's cumulative announce
// counters, kept so the bench tick can turn them into a per-second rate.
type announceTick struct {
	sent, failed uint64
	at           time.Time
}

// benchAnnounceLast holds the last read per engine scope. Only the bench
// ticker touches it, one goroutine, so no lock.
var benchAnnounceLast = map[string]announceTick{}

// addAnnounceRates fills the announce rate columns from the delta since the
// previous tick. A scope with no previous read is skipped entirely (0 in the
// row) instead of reporting the since-boot total over one interval.
func addAnnounceRates(snap map[string]interface{}) {
	now := time.Now()
	for _, scope := range []string{"race", "hoard"} {
		sent, failed, _ := engine.AnnounceStats(scope)
		prev, seen := benchAnnounceLast[scope]
		benchAnnounceLast[scope] = announceTick{sent: sent, failed: failed, at: now}
		if !seen {
			continue
		}
		dt := now.Sub(prev.at).Seconds()
		// Counters are monotone; a lower read would mean a restart, which
		// cannot happen in-process, but guard rather than emit a negative rate.
		if dt <= 0 || sent < prev.sent || failed < prev.failed {
			continue
		}
		snap[scope+"_announce_rate"] = float64(sent-prev.sent) / dt
		snap[scope+"_announce_fail_rate"] = float64(failed-prev.failed) / dt
	}
}

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
	// Announce cadence: the counters are cumulative, the graph wants a rate.
	// First tick after a start has no previous read, so it emits nothing
	// rather than a spike of "everything since boot divided by one interval".
	addAnnounceRates(snap)

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

func (a *raceAPIAdapter) SampleServedInfoHash() string { return a.engine.SampleServedInfoHash() }

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
func (a *raceAPIAdapter) MatchHashes(f engine.TorrentFilter, exclude map[string]bool) []string {
	return a.engine.MatchHashes(f, exclude)
}
func (a *raceAPIAdapter) GetTorrentDetail(infoHash string) map[string]interface{} {
	return a.engine.GetTorrentDetail(infoHash)
}
func (a *raceAPIAdapter) GetTorrentFileList(infoHash string) []map[string]interface{} {
	return a.engine.GetTorrentFileList(infoHash)
}

func (a *raceAPIAdapter) GetTorrentAvailability(infoHash string) map[string]interface{} {
	return a.engine.GetTorrentAvailability(infoHash)
}

func (a *raceAPIAdapter) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	return a.engine.SetEngineOptFlag(name, on, value)
}

func (a *raceAPIAdapter) InboundAccepted() (int64, error) { return a.engine.InboundAccepted() }

func (a *raceAPIAdapter) EngineOptFlags() (map[string]interface{}, error) {
	return a.engine.EngineOptFlags()
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
func (a *raceAPIAdapter) ReannnounceTorrent(infoHash string) bool {
	return a.engine.ReannounceNow(infoHash)
}
func (a *raceAPIAdapter) AddTrackerToTorrent(infoHash, url string) error {
	return a.engine.AddTrackerToTorrent(infoHash, url)
}
func (a *raceAPIAdapter) GetTrackerTiers(infoHash string) ([][]string, error) {
	return a.engine.GetTrackerTiers(infoHash)
}
func (a *raceAPIAdapter) SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error) {
	return a.engine.SetTrackerTiers(infoHash, tiers)
}
func (a *raceAPIAdapter) TorrentFilePath(infoHash string) (string, bool) {
	return a.engine.TorrentFilePath(infoHash)
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
func (a *raceAPIAdapter) ListenPort() int { return a.engine.ListenPort() }

func (a *raceAPIAdapter) SetListenPort(port int) error {
	return a.engine.SetListenPort(port)
}
func (a *raceAPIAdapter) HasTorrent(infoHash string) bool {
	return a.engine.GetTorrentDetail(infoHash) != nil
}
func (a *raceAPIAdapter) SessionGrabbed() int64 { return a.engine.SessionGrabbed() }
func (a *raceAPIAdapter) AggregateStats() map[string]interface{} {
	return a.engine.AggregateStats()
}

type hoardAPIAdapter struct{ engine *engine.HoardEngine }

func (a *hoardAPIAdapter) SampleServedInfoHash() string { return a.engine.SampleServedInfoHash() }

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
func (a *hoardAPIAdapter) GetTorrentListInCategory(category string) []map[string]interface{} {
	// The engine filters while it walks its own cache, so neither the copy of
	// the 196k structs nor the maps for the rows we would drop ever happen.
	list := a.engine.GetTorrentListInCategory(category)
	result := make([]map[string]interface{}, 0, len(list))
	for i := range list {
		result = append(result, torrentStatsToMap(&list[i]))
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

func (a *hoardAPIAdapter) GetTorrentAvailability(infoHash string) map[string]interface{} {
	return a.engine.GetTorrentAvailability(infoHash)
}

func (a *hoardAPIAdapter) SetEngineOptFlag(name string, on bool, value int64) (map[string]interface{}, error) {
	return a.engine.SetEngineOptFlag(name, on, value)
}

func (a *hoardAPIAdapter) InboundAccepted() (int64, error) { return a.engine.InboundAccepted() }

func (a *hoardAPIAdapter) EngineOptFlags() (map[string]interface{}, error) {
	return a.engine.EngineOptFlags()
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
func (a *hoardAPIAdapter) ReannnounceTorrent(infoHash string) bool {
	return a.engine.ReannounceNow(infoHash)
}
func (a *hoardAPIAdapter) AddTrackerToTorrent(infoHash, url string) error {
	return a.engine.AddTrackerToTorrent(infoHash, url)
}
func (a *hoardAPIAdapter) GetTrackerTiers(infoHash string) ([][]string, error) {
	return a.engine.GetTrackerTiers(infoHash)
}
func (a *hoardAPIAdapter) SetTrackerTiers(infoHash string, tiers [][]string) ([][]string, error) {
	return a.engine.SetTrackerTiers(infoHash, tiers)
}
func (a *hoardAPIAdapter) TorrentFilePath(infoHash string) (string, bool) {
	return a.engine.TorrentFilePath(infoHash)
}
func (a *hoardAPIAdapter) ListenPort() int { return a.engine.ListenPort() }

func (a *hoardAPIAdapter) SetListenPort(port int) error { return a.engine.SetListenPort(port) }
func (a *hoardAPIAdapter) SetAddedTime(infoHash string, t time.Time) {
	a.engine.SetAddedTime(infoHash, t)
}
func (a *hoardAPIAdapter) SetCompletedTime(infoHash string, t time.Time) {
	a.engine.SetCompletedTime(infoHash, t)
}
func (a *hoardAPIAdapter) HasTorrent(infoHash string) bool {
	return a.engine.GetTorrentDetail(infoHash) != nil
}
func (a *hoardAPIAdapter) PauseAll() int { return a.engine.PauseAll() }
func (a *hoardAPIAdapter) SetUserPaused(ih string, paused bool) error {
	return a.engine.SetUserPaused(ih, paused)
}
func (a *hoardAPIAdapter) MatchHashes(f engine.TorrentFilter, exclude map[string]bool) []string {
	return a.engine.MatchHashes(f, exclude)
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
func (a *benchAPIAdapter) InsertRaceEvent(ev bench.RaceEvent) { a.db.InsertRaceEvent(ev) }
func (a *benchAPIAdapter) InsertRaceSnapshots(snapshots []bench.RaceSnapshot) {
	a.db.InsertRaceSnapshots(snapshots)
}
func (a *benchAPIAdapter) PurgeOld() int64 { return a.db.PurgeOld() }

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
		"tracker_host": s.TrackerHost, "seeding_time": s.SeedingTime,
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
	// A front-only node never announces, but it composes what its agents
	// announce with, so it has to load its own [announce_*] tables: without
	// this it would push empty override maps over every agent's spoofing.
	api.InitAnnounceOverrides(cfg)
	resumeJobs := startFrontOnlyJobs(ctx, cfg, apiServer)
	startFrontOnlyBench(ctx, cfg, apiServer)
	apiServer.StartAgentReconciler(ctx)
	// Only once the agents are registered: a move resolves both of its ends by
	// name when it runs, and resuming before the dial fails it with "no agent
	// named ..." -- the restart the job was built to survive killing it.
	if resumeJobs != nil {
		resumeJobs()
	}
	go func() {
		if err := apiServer.Run(); err != nil {
			slog.Error("API server error", "error", err)
		}
	}()
	slog.Info("front-only API started", "addr", fmt.Sprintf("http://%s:%d", cfg.Daemon.APIHost, cfg.Daemon.APIPort))
	// A front-only node restores nothing of its own: there is no local engine
	// and no local state to page back in, so it is fully started the moment the
	// API is up. Leaving the flag alone -- as this mode used to -- pins
	// /api/startup at {"ready":false} forever, and the web UI polls that flag
	// to lift its splash overlay: the whole UI stayed unreachable behind
	// "Initializing...", with a backend that was answering every other route
	// correctly. Deliberately NOT gated on the agents being reachable: agent
	// health is reported by /api/agents, and blocking the UI on it would put
	// the same permanent overlay back the moment an agent was down.
	api.SetStartupReady(true)
	<-ctx.Done()
}

// startFrontOnlyBench opens this node's bench DB and starts sampling the agents'
// race engines into it.
//
// The timeline is fed by an engine's own events and a 5s sampler in the
// monolith. A controller has neither, so the panel had no data for any torrent
// on an agent -- and the agents cannot fill it either: they hold no bench DB
// and no history. The node that talks to all of them does.
func startFrontOnlyBench(ctx context.Context, cfg *config.HydraConfig, apiServer *api.Server) {
	db := bench.NewBenchDB(filepath.Join(cfg.Daemon.DataDir, "bench.db"))
	if err := db.Open(); err != nil {
		slog.Warn("bench: open failed, the race timeline stays empty on this node", "error", err)
		return
	}
	context.AfterFunc(ctx, func() { db.Close() })
	apiServer.SetBenchDB(&benchAPIAdapter{db: db})
	apiServer.StartRaceTimelineRecorder(ctx)
}

// startFrontOnlyJobs gives a controller node its background job manager and
// returns the resume hook, or nil when the store could not be opened.
//
// A controller runs no engine, but a cross-agent move is its work by
// definition: both ends are agents and it is the only node connected to both.
// Without this the Jobs view answers 503 on a node where moving payload
// between agents is the whole point, and /api/jobs/move-remote cannot be
// submitted at all.
//
// The store here holds the job table and nothing else -- it is deliberately
// NOT handed to api.SetStore: this node hosts no torrents, and an empty
// torrent table presented as the truth would have the counters and listings
// report zero rather than say they live on the agents.
func startFrontOnlyJobs(ctx context.Context, cfg *config.HydraConfig, apiServer *api.Server) func() {
	st, err := store.Open(filepath.Join(cfg.Daemon.DataDir, "hydra.db"))
	if err != nil {
		slog.Error("store: open failed, background jobs are off on this node", "error", err)
		return nil
	}
	context.AfterFunc(ctx, func() { st.Close() })

	// Both ends are named agents, resolved at RUN time: a resumed job must
	// find the agent as it is now, not as it was when the job was created.
	// "local" is not a node here, and saying so beats dialing nothing.
	dial := func(agent, engineID string) (*grpcclient.Client, error) {
		if agent == "" || agent == api.LocalAgentName {
			return nil, fmt.Errorf("this node is a controller and hosts no torrents: name the agent instead of %q", api.LocalAgentName)
		}
		return apiServer.RemoteAgentEngineClient(agent, engineID)
	}

	jobMgr := jobs.NewManager(ctx, st, 1)
	jobMgr.Register(&jobs.RemoteMoveRunner{
		DialSource: func(p jobs.RemoteMoveParams) (jobs.PieceSource, error) {
			cl, derr := dial(p.SourceAgent, p.Engine)
			if derr != nil {
				return nil, derr
			}
			return cl, nil
		},
		DialSink: func(p jobs.RemoteMoveParams) (jobs.PieceSink, error) {
			cl, derr := dial(p.TargetAgent, p.Engine)
			if derr != nil {
				return nil, derr
			}
			return cl, nil
		},
		ResolveSavePath: apiServer.CategorySavePathFor,
		SetTargetCategory: func(p jobs.RemoteMoveParams, infoHash string) error {
			cl, derr := dial(p.TargetAgent, p.Engine)
			if derr != nil {
				return derr
			}
			return cl.SetCategoryLabel(p.Engine, infoHash, p.Category)
		},
		FreeSpace: func(agent, path string) (int64, error) {
			cl, derr := dial(agent, agentwire.EngineHoard)
			if derr != nil {
				return 0, derr
			}
			return cl.DiskFree(path)
		},
	})
	apiServer.SetJobManager(jobMgr)
	// Finished jobs are kept long enough to be useful and not forever.
	jobMgr.Prune(30 * 24 * time.Hour)
	return jobMgr.ResumeAll
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
func (a *raceAPIAdapter) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error) {
	return a.engine.FetchMetadata(infoHash, trackers, peers, bindingID)
}
func (a *raceAPIAdapter) GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error) {
	return a.engine.GetMetadata(infoHash)
}
func (a *hoardAPIAdapter) FetchMetadata(infoHash string, trackers, peers []string, bindingID *uint32) (*ltclient.FetchMetadataResult, error) {
	return a.engine.FetchMetadata(infoHash, trackers, peers, bindingID)
}
func (a *hoardAPIAdapter) GetMetadata(infoHash string) (*ltclient.GetMetadataResult, error) {
	return a.engine.GetMetadata(infoHash)
}

// The handoff surface is a straight pass-through: the adapters exist to keep
// the api package from importing the engine types, not to add behaviour.
func (a *raceAPIAdapter) Role() engine.Role { return a.engine.Role() }
func (a *raceAPIAdapter) ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error) {
	return a.engine.ExportTorrentState(infoHash)
}
func (a *raceAPIAdapter) AdoptTorrent(rec *ltclient.ResumeRecord, category string) error {
	return a.engine.AdoptTorrent(rec, category)
}
func (a *raceAPIAdapter) ReleaseTorrent(infoHash string) error {
	return a.engine.ReleaseTorrent(infoHash)
}

func (a *hoardAPIAdapter) Role() engine.Role { return a.engine.Role() }
func (a *hoardAPIAdapter) ExportTorrentState(infoHash string) (*ltclient.ResumeRecord, error) {
	return a.engine.ExportTorrentState(infoHash)
}
func (a *hoardAPIAdapter) AdoptTorrent(rec *ltclient.ResumeRecord, category string) error {
	return a.engine.AdoptTorrent(rec, category)
}
func (a *hoardAPIAdapter) ReleaseTorrent(infoHash string) error {
	return a.engine.ReleaseTorrent(infoHash)
}
