package main

import (
	"fmt"
	"log/slog"
	"os"
	"strconv"
	"strings"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/config"
)

// An agent node knows four things about itself and takes everything else from
// its front: which engines it hosts (id + role) and, per engine, the port it
// listens on and whether it does IPv6. Those four are the agent's because they
// are facts about the machine -- which port its VPN forwards, which families
// its interfaces carry -- and a front cannot know them.
//
// Each is settable three ways, resolved flag > env > TOML > default, the same
// precedence resolveAgentToken uses for the agent token. Two env spellings are
// offered: HYDRA_ENGINE_* for the one-engine-per-container case, which is what
// most deployments run, and HYDRA_ENGINES for a node hosting several.
const (
	envEngines          = "HYDRA_ENGINES"
	envEngineID         = "HYDRA_ENGINE_ID"
	envEngineRole       = "HYDRA_ENGINE_ROLE"
	envEngineListenPort = "HYDRA_ENGINE_LISTEN_PORT"
	envEngineEnableIPv6 = "HYDRA_ENGINE_ENABLE_IPV6"
	envAgentAddr        = "HYDRA_AGENT_ADDR"
	envDataDir          = "HYDRA_DATA_DIR"
	envAgentTLSCert     = "HYDRA_AGENT_TLS_CERT"
	envAgentTLSKey      = "HYDRA_AGENT_TLS_KEY"
	envListenPortHook   = "HYDRA_LISTEN_PORT_HOOK"
	envHealthAddr       = "HYDRA_HEALTH_ADDR"
)

// agentBootEngine is one engine's identity: everything the node decides for
// itself. The session configuration that goes with it comes from the front.
type agentBootEngine struct {
	ID         string
	Role       string
	ListenPort int
	EnableIPv6 bool
}

func (e agentBootEngine) descriptor() agentwire.EngineDescriptor {
	return agentwire.EngineDescriptor{ID: e.ID, Role: e.Role}
}

// engineSpecFlag collects repeated --engine values.
type engineSpecFlag []string

func (f *engineSpecFlag) String() string { return strings.Join(*f, ";") }

func (f *engineSpecFlag) Set(v string) error {
	*f = append(*f, v)
	return nil
}

// parseEngineSpec reads one "id=race-0,role=race,port=12314,ipv6=true" spec.
// An unknown key is an error rather than a shrug: a typo that fell through
// would leave the engine on a default port, announced to every tracker.
func parseEngineSpec(spec string) (agentBootEngine, error) {
	out := agentBootEngine{}
	for _, field := range strings.Split(spec, ",") {
		field = strings.TrimSpace(field)
		if field == "" {
			continue
		}
		k, v, ok := strings.Cut(field, "=")
		if !ok {
			return out, fmt.Errorf("field %q is not key=value", field)
		}
		k, v = strings.TrimSpace(strings.ToLower(k)), strings.TrimSpace(v)
		switch k {
		case "id":
			out.ID = v
		case "role":
			out.Role = strings.ToLower(v)
		case "port", "listen_port":
			n, err := strconv.Atoi(v)
			if err != nil {
				return out, fmt.Errorf("port %q is not a number", v)
			}
			out.ListenPort = n
		case "ipv6", "enable_ipv6":
			b, err := strconv.ParseBool(v)
			if err != nil {
				return out, fmt.Errorf("ipv6 %q is not a boolean", v)
			}
			out.EnableIPv6 = b
		default:
			return out, fmt.Errorf("unknown field %q (want id, role, port, ipv6)", k)
		}
	}
	return out, nil
}

// parseEngineSpecs reads the ';'-separated multi-engine form.
func parseEngineSpecs(specs []string) ([]agentBootEngine, error) {
	var out []agentBootEngine
	for _, group := range specs {
		for _, spec := range strings.Split(group, ";") {
			if strings.TrimSpace(spec) == "" {
				continue
			}
			e, err := parseEngineSpec(spec)
			if err != nil {
				return nil, fmt.Errorf("engine spec %q: %w", spec, err)
			}
			out = append(out, e)
		}
	}
	return out, nil
}

// envEngineSpecs renders the environment's engine identity in the spec syntax,
// so the env and flag forms go through one parser and cannot drift apart.
func envEngineSpecs() []string {
	if v := strings.TrimSpace(os.Getenv(envEngines)); v != "" {
		return []string{v}
	}
	id := strings.TrimSpace(os.Getenv(envEngineID))
	role := strings.TrimSpace(os.Getenv(envEngineRole))
	if id == "" && role == "" {
		return nil
	}
	fields := []string{"id=" + id, "role=" + role}
	if p := strings.TrimSpace(os.Getenv(envEngineListenPort)); p != "" {
		fields = append(fields, "port="+p)
	}
	if v := strings.TrimSpace(os.Getenv(envEngineEnableIPv6)); v != "" {
		fields = append(fields, "ipv6="+v)
	}
	return []string{strings.Join(fields, ",")}
}

// agentIdentityFromArgs reports whether the node's identity comes from flags or
// the environment, which is also what says it needs no config file at all.
func agentIdentityFromArgs(flagSpecs []string) bool {
	return len(flagSpecs) > 0 || len(envEngineSpecs()) > 0
}

// resolveAgentBoot resolves the node's engine identity, returning the engines
// and where they came from ("flag", "env" or "file").
func resolveAgentBoot(flagSpecs []string, cfg *config.HydraConfig) ([]agentBootEngine, string, error) {
	if len(flagSpecs) > 0 {
		e, err := parseEngineSpecs(flagSpecs)
		return e, "flag", err
	}
	if specs := envEngineSpecs(); len(specs) > 0 {
		e, err := parseEngineSpecs(specs)
		return e, "env", err
	}
	engines, err := cfg.ResolveEngines()
	if err != nil {
		return nil, "file", err
	}
	out := make([]agentBootEngine, 0, len(engines))
	for _, ec := range engines {
		out = append(out, agentBootEngine{
			ID: ec.ID, Role: ec.Role, ListenPort: ec.ListenPort, EnableIPv6: ec.EnableIPv6,
		})
	}
	return out, "file", nil
}

// validateAgentBoot rejects an identity the node cannot run. Every check here
// is a failure that would otherwise be silent and expensive: an engine with no
// id cannot be addressed by the front, an unknown role picks neither engine
// implementation, and two engines on one port leave the second one dead.
func validateAgentBoot(engines []agentBootEngine) error {
	if len(engines) == 0 {
		return fmt.Errorf("no engine declared: pass --engine id=...,role=... or set $%s / $%s", envEngineID, envEngines)
	}
	seenID := make(map[string]bool, len(engines))
	seenPort := make(map[int]string, len(engines))
	for i, e := range engines {
		if e.ID == "" {
			return fmt.Errorf("engine %d: empty id", i)
		}
		if e.Role != "race" && e.Role != "hoard" {
			return fmt.Errorf("engine %q: role must be \"race\" or \"hoard\", got %q", e.ID, e.Role)
		}
		if seenID[e.ID] {
			return fmt.Errorf("duplicate engine id %q", e.ID)
		}
		seenID[e.ID] = true
		if e.ListenPort < 0 || e.ListenPort > 65535 {
			return fmt.Errorf("engine %q: listen port %d out of range (1-65535)", e.ID, e.ListenPort)
		}
		if e.ListenPort != 0 {
			if other, dup := seenPort[e.ListenPort]; dup {
				return fmt.Errorf("engine %q: listen port %d already used by engine %q", e.ID, e.ListenPort, other)
			}
			seenPort[e.ListenPort] = e.ID
		}
	}
	return nil
}

// logAgentBoot records the resolved identity and where it came from. With
// three possible sources, "which port did it actually take" has to be
// answerable from the log alone.
func logAgentBoot(engines []agentBootEngine, source string) {
	for _, e := range engines {
		slog.Info("agent engine identity", "source", source, "id", e.ID, "role", e.Role,
			"listen_port", e.ListenPort, "enable_ipv6", e.EnableIPv6)
	}
}

// envOr returns the environment value when the flag was left at its default.
func envOr(flagValue, envName string) string {
	if flagValue != "" {
		return flagValue
	}
	return strings.TrimSpace(os.Getenv(envName))
}

// envIntOr is envOr for an int flag. An unparseable value is reported rather
// than silently treated as "off".
func envIntOr(flagValue int, envName string) int {
	if flagValue != 0 {
		return flagValue
	}
	v := strings.TrimSpace(os.Getenv(envName))
	if v == "" {
		return 0
	}
	n, err := strconv.Atoi(v)
	if err != nil {
		slog.Warn("ignoring unparseable environment value", "var", envName, "value", v, "err", err)
		return 0
	}
	return n
}

// warnIgnoredAgentSections tells the operator which parts of an agent's config
// file this node does not read. In the front-driven model the session tuning,
// the drain, the web UI login and the announce overrides all come from the
// front, so a value left behind in the agent's file is not applied anywhere --
// visible drift beats a setting that looks live and is not.
func warnIgnoredAgentSections(cfg *config.HydraConfig, identitySource string) {
	if cfg.SourcePath == "" || identitySource == "file" {
		return
	}
	var ignored []string
	if cfg.Race.ListenPort != 0 || cfg.Race.MaxConnections != 0 {
		ignored = append(ignored, "[race]")
	}
	if cfg.Hoard.ListenPort != 0 || cfg.Hoard.MaxConnections != 0 {
		ignored = append(ignored, "[hoard]")
	}
	if len(cfg.Engines) > 0 {
		ignored = append(ignored, "[[engine]]")
	}
	if len(cfg.Agents) > 0 {
		ignored = append(ignored, "[[agent]]")
	}
	if cfg.Auth.PasswordHash != "" {
		ignored = append(ignored, "[auth]")
	}
	if cfg.RaceDrain.RacePath != "" {
		ignored = append(ignored, "[race_drain]")
	}
	if cfg.VpnSpeedtest.Enabled {
		ignored = append(ignored, "[vpn_speedtest]")
	}
	if cfg.ArrCleanup.RadarrURL != "" || cfg.ArrCleanup.SonarrURL != "" {
		ignored = append(ignored, "[arr_cleanup]")
	}
	if len(ignored) > 0 {
		slog.Warn("agent-only: these config sections are ignored, this node takes them from its front",
			"path", cfg.SourcePath, "sections", strings.Join(ignored, " "))
	}
}
