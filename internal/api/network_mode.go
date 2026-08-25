package api

import (
	"context"
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/url"
	"os"
	"strconv"
	"strings"
	"time"

	"github.com/gin-gonic/gin"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/engine"
)

// The network tab exists because the flat settings list cannot express the one
// thing that matters here: these setups are mutually exclusive. Nobody runs a
// VPN and a SOCKS5 relay at once, yet the config file happily holds the keys of
// both, and a half-abandoned attempt from last week is indistinguishable from a
// deliberate setup. Picking a mode writes its keys AND clears the others, so
// what the file says is what the operator chose.

const (
	netModeDirect  = "direct"
	netModeGluetun = "gluetun"
	netModeSocks5  = "socks5"
	netModeProxyV2 = "proxy_v2"
)

// The mode formerly called "vpn" is now "gluetun", and the interface binding it
// owned moved down into "direct". The rename is not cosmetic: nothing in that
// mode was ever about VPNs in general. What separated it from direct was the
// presence of bind_interface, and what it uniquely knows how to do is read a
// forwarded port off a gluetun control server. A bare-metal WireGuard host has
// an interface and no gluetun, and had no honest mode to sit in.
//
// Direct now carries a bind_interface PER ENGINE, which is the setup people
// actually asked for: one tunnel per engine, each engine's peers AND announces
// leaving by its own interface. Empty means the kernel default route, so a
// no-VPN host is the same direct mode with nothing filled in.

// netModeFields is the union of every mode's inputs. Each mode reads the few it
// needs and the handler blanks the rest, so a field left over from another mode
// can never survive a save.
type netModeFields struct {
	RaceListenPort     int      `json:"race_listen_port"`
	HoardListenPort    int      `json:"hoard_listen_port"`
	EnableIPv6         bool     `json:"enable_ipv6"`
	RaceBindInterface  string   `json:"race_bind_interface"`
	HoardBindInterface string   `json:"hoard_bind_interface"`
	Socks5Host         string   `json:"socks5_host"`
	Socks5Port         int      `json:"socks5_port"`
	Socks5User         string   `json:"socks5_user"`
	Socks5Pass         string   `json:"socks5_pass"`
	RaceProxyV2Port    int      `json:"race_proxy_v2_port"`
	HoardProxyV2Port   int      `json:"hoard_proxy_v2_port"`
	ProxyV2Addr        string   `json:"proxy_v2_listen_addr"`
	ProxyV2Trusted     []string `json:"proxy_v2_trusted_sources"`
	GluetunPort        bool     `json:"gluetun_port_forward"`
	GluetunURL         string   `json:"gluetun_url"`
	GluetunAPIKey      string   `json:"gluetun_api_key"`
	GluetunEngine      string   `json:"gluetun_port_engine"`
}

// envOverride names an environment variable that silently outranks a field on
// this page. Without this block the tab would confidently show a setting the
// daemon is not using — the same class of lie the announce leak was.
type envOverride struct {
	Name   string `json:"name"`
	Value  string `json:"value"`
	Effect string `json:"effect"`
}

type netModeResponse struct {
	Mode     string        `json:"mode"`
	Fields   netModeFields `json:"fields"`
	Env      []envOverride `json:"env_overrides"`
	Warnings []string      `json:"warnings"`
}

func sectionOf(m map[string]interface{}, name string) map[string]interface{} {
	if v, ok := m[name].(map[string]interface{}); ok {
		return v
	}
	return map[string]interface{}{}
}

func tomlStr(sec map[string]interface{}, key string) string {
	if v, ok := sec[key].(string); ok {
		return v
	}
	return ""
}

func tomlInt(sec map[string]interface{}, key string) int {
	switch v := sec[key].(type) {
	case int64:
		return int(v)
	case float64:
		return int(v)
	case int:
		return v
	}
	return 0
}

func tomlBool(sec map[string]interface{}, key string) bool {
	b, _ := sec[key].(bool)
	return b
}

func tomlStrList(sec map[string]interface{}, key string) []string {
	raw, ok := sec[key].([]interface{})
	if !ok {
		return nil
	}
	out := make([]string, 0, len(raw))
	for _, v := range raw {
		if s, ok := v.(string); ok && strings.TrimSpace(s) != "" {
			out = append(out, s)
		}
	}
	return out
}

// detectNetMode derives the mode from the config rather than storing it. A
// stored mode would be a second source of truth, and the first time someone
// edits the TOML by hand it would start lying — which is the failure this whole
// tab is meant to remove, not reproduce.
func detectNetMode(race, hoard map[string]interface{}) string {
	if tomlInt(race, "listen_port_proxy_v2") > 0 || tomlInt(hoard, "listen_port_proxy_v2") > 0 {
		return netModeProxyV2
	}
	for _, sec := range []map[string]interface{}{race, hoard} {
		if tomlStr(sec, "socks5_outbound_host") != "" || tomlStr(sec, "announce_proxy") != "" {
			return netModeSocks5
		}
	}
	// bind_interface deliberately does NOT appear here any more. It used to be
	// what told vpn mode from direct, but now both modes carry it, so keying on
	// it would read every bare-metal WireGuard setup as gluetun and the tab
	// would offer a control-server URL to a host that has no gluetun. What is
	// unique to this mode is talking to gluetun, so that is what identifies it.
	for _, sec := range []map[string]interface{}{race, hoard} {
		if tomlBool(sec, "gluetun_port_forward") || tomlStr(sec, "gluetun_url") != "" {
			return netModeGluetun
		}
	}
	return netModeDirect
}

// bindInterfaceFor is the interface one engine leaves by. Per-engine because
// that is the whole point of the change: a monolith can hold several tunnels,
// one per engine, and a single shared field cannot express that.
func bindInterfaceFor(f netModeFields, scope string) string {
	if scope == "race" {
		return strings.TrimSpace(f.RaceBindInterface)
	}
	return strings.TrimSpace(f.HoardBindInterface)
}

// looksLikeTunnel is a naming heuristic, deliberately advisory: the set of VPN
// clients is open-ended, so a wrong guess must not block a working setup. It
// exists because the interface picker lists every interface on the host and the
// ordinary one is often the first that looks plausible.
func looksLikeTunnel(name string) bool {
	n := strings.ToLower(strings.TrimSpace(name))
	for _, p := range []string{"tun", "tap", "wg", "ppp", "vpn", "utun", "tailscale", "proton", "nordlynx", "ipsec"} {
		if strings.HasPrefix(n, p) {
			return true
		}
	}
	return false
}

// gluetunEngineOf names the engine that follows the forwarded port. A
// provider hands out exactly one, so pointing both engines at it would have
// the second fail to bind; the operator picks which one gets it. Empty means
// hoard, which is what every config written before this choice existed meant.
func gluetunEngineOf(f netModeFields) string {
	if strings.TrimSpace(f.GluetunEngine) == "race" {
		return "race"
	}
	return "hoard"
}

// otherEngine is the one left on its configured port, and therefore the one
// the warnings have to name: saying "the race engine stays unreachable" while
// the race is the one holding the port is worse than saying nothing.
func otherEngine(scope string) string {
	if scope == "race" {
		return "hoard"
	}
	return "race"
}

func maskProxyURL(raw string) string {
	u, err := url.Parse(raw)
	if err != nil || u.User == nil {
		return raw
	}
	if _, hasPass := u.User.Password(); hasPass {
		u.User = url.UserPassword(u.User.Username(), "***")
	}
	return u.String()
}

func collectEnvOverrides() []envOverride {
	var out []envOverride
	add := func(name, effect string) {
		if v := strings.TrimSpace(os.Getenv(name)); v != "" {
			out = append(out, envOverride{Name: name, Value: maskProxyURL(v), Effect: effect})
		}
	}
	add("TYPHON_ANNOUNCE_PROXY", "Announces go through this proxy for every engine that has no announce_proxy of its own.")
	add("TYPHON_ANNOUNCE_V6_PROXY", "Fires a second announce through this proxy.")
	add("TYPHON_SELF_IPS", "Addresses treated as our own, so we never dial ourselves.")
	add("TYPHON_DISABLE_UTP", "uTP is off: peers are reached over TCP only.")
	return out
}

// modeWarnings lists what is inconsistent in the CONFIG alone — no network is
// touched, so this is cheap enough to return on every page load. The live
// measurements live in the check endpoint.
func modeWarnings(mode string, f netModeFields, env []envOverride) []string {
	var w []string
	proxied := mode == netModeSocks5 || mode == netModeProxyV2
	if mode == netModeSocks5 {
		// A plain SOCKS5 proxy carries outgoing connections only. RFC 1928 has
		// a BIND command for the other direction, but servers essentially never
		// implement it and Hydra does not use it. The tracker still publishes
		// the proxy address with our local listen port, and that pair answers
		// nobody: the setup looks complete and quietly halves what it can do.
		w = append(w, "A plain SOCKS5 proxy only carries outgoing connections. Nobody can reach you through it, so the address you announce answers no one and you only trade with peers you connect to yourself. Use the relay mode if you want to stay reachable.")
	}
	if proxied {
		w = append(w, "UDP trackers are skipped in this mode: SOCKS5 carries TCP only, and a direct datagram would hand the tracker the address you are hiding.")
	}
	if mode == netModeGluetun && f.GluetunPort {
		eng := gluetunEngineOf(f)
		w = append(w, "The "+eng+" engine takes its listen port from gluetun and holds its announces until it has one. A provider forwards a single port, so the "+otherEngine(eng)+" engine keeps its own and stays unreachable.")
	}
	race, hoard := bindInterfaceFor(f, "race"), bindInterfaceFor(f, "hoard")
	if mode == netModeGluetun {
		for _, e := range []struct{ scope, name string }{{"race", race}, {"hoard", hoard}} {
			if e.name != "" && !looksLikeTunnel(e.name) {
				w = append(w, "The "+e.scope+" engine is bound to \""+e.name+"\", which does not look like a VPN tunnel (those are usually named tun0, wg0 or similar). Its traffic leaves outside the tunnel, or does not leave at all.")
			}
			if e.name == "" {
				w = append(w, "The "+e.scope+" engine is bound to no interface, so its peers and its announces leave by the host's default route: outside the tunnel, under the address the tunnel exists to hide.")
			}
		}
	}
	// The half-configured case is the dangerous one, and it only exists now that
	// the two engines can differ. One engine inside a tunnel and the other on
	// the default route looks entirely healthy on the page that shows a tunnel.
	if mode == netModeDirect && (race == "") != (hoard == "") {
		bound, bare := "race", "hoard"
		if race == "" {
			bound, bare = "hoard", "race"
		}
		w = append(w, "Only the "+bound+" engine is bound to an interface. The "+bare+" engine leaves by the host's default route, so its peers and its announces publish this host's own address.")
	}
	if mode == netModeProxyV2 && len(f.ProxyV2Trusted) == 0 {
		w = append(w, "No trusted source for the PROXY-v2 listener: anyone who reaches that port could forge any peer address. List your relay's address.")
	}
	if mode == netModeProxyV2 && f.RaceProxyV2Port == 0 && f.HoardProxyV2Port == 0 {
		w = append(w, "PROXY-v2 mode with no PROXY-v2 port on either engine: inbound peers still arrive on the plain listener.")
	}
	for _, e := range env {
		if e.Name == "TYPHON_ANNOUNCE_PROXY" && !proxied {
			w = append(w, "TYPHON_ANNOUNCE_PROXY is set in the environment: announces go through it whatever this page shows.")
		}
	}
	return w
}

func (s *Server) readNetworkConfig() (map[string]interface{}, error) {
	data, err := os.ReadFile(s.settingsFilePath())
	if err != nil {
		return nil, err
	}
	return config.ParseTOMLMap(data)
}

func (s *Server) handleNetworkModeGet(c *gin.Context) {
	m, err := s.readNetworkConfig()
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	race, hoard := sectionOf(m, "race"), sectionOf(m, "hoard")
	mode := detectNetMode(race, hoard)

	f := netFieldsFromTOML(race, hoard)

	env := collectEnvOverrides()
	warn := modeWarnings(mode, f, env)
	if tomlStr(race, "socks5_outbound_host") != tomlStr(hoard, "socks5_outbound_host") {
		warn = append(warn, "The two engines have different SOCKS5 hosts in the config file. Saving here sets both to the same value.")
	}
	c.JSON(http.StatusOK, netModeResponse{Mode: mode, Fields: f, Env: env, Warnings: warn})
}

// netFieldsFromTOML reads the whole page out of the config, once.
//
// It is a function and not two hand-written literals because it used to be two:
// this handler filled every field, and the check endpoint filled the three it
// happened to need. Adding a warning that reads a fourth made the check report
// "bound to no interface" about two engines that were both bound to tun1 -- a
// confident, specific, entirely false statement, produced by a struct nobody
// had finished filling in. One reader, so a field added later cannot reach only
// half of its callers.
//
// The proxy credentials are one setting shown once, even though they live under
// both engines: a per-engine proxy is exactly the split that let the announce
// leak hide. Hoard wins when the two disagree, and the mismatch is reported
// rather than quietly flattened.
func netFieldsFromTOML(race, hoard map[string]interface{}) netModeFields {
	f := netModeFields{
		RaceListenPort:  tomlInt(race, "listen_port"),
		HoardListenPort: tomlInt(hoard, "listen_port"),
		EnableIPv6:      tomlBool(race, "enable_ipv6") || tomlBool(hoard, "enable_ipv6"),
		// Read per engine, never collapsed. The TOML has always held one key
		// per section; it was this page that flattened the two into a single
		// field with firstNonEmpty and made a split setup unrepresentable.
		RaceBindInterface:  tomlStr(race, "bind_interface"),
		HoardBindInterface: tomlStr(hoard, "bind_interface"),
		GluetunPort:        tomlBool(race, "gluetun_port_forward") || tomlBool(hoard, "gluetun_port_forward"),
		// The section the flag is true in IS the choice: a separate "which
		// engine" key would be a second source of truth, wrong the first time
		// someone edits the file by hand.
		GluetunEngine:    gluetunEngineFromTOML(race),
		GluetunURL:       firstNonEmpty(tomlStr(hoard, "gluetun_url"), tomlStr(race, "gluetun_url")),
		GluetunAPIKey:    firstNonEmpty(tomlStr(hoard, "gluetun_api_key"), tomlStr(race, "gluetun_api_key")),
		Socks5Host:       firstNonEmpty(tomlStr(hoard, "socks5_outbound_host"), tomlStr(race, "socks5_outbound_host")),
		Socks5User:       firstNonEmpty(tomlStr(hoard, "socks5_outbound_user"), tomlStr(race, "socks5_outbound_user")),
		Socks5Pass:       firstNonEmpty(tomlStr(hoard, "socks5_outbound_pass"), tomlStr(race, "socks5_outbound_pass")),
		RaceProxyV2Port:  tomlInt(race, "listen_port_proxy_v2"),
		HoardProxyV2Port: tomlInt(hoard, "listen_port_proxy_v2"),
		ProxyV2Addr:      firstNonEmpty(tomlStr(hoard, "listen_addr_proxy_v2"), tomlStr(race, "listen_addr_proxy_v2")),
	}
	if p := tomlInt(hoard, "socks5_outbound_port"); p > 0 {
		f.Socks5Port = p
	} else {
		f.Socks5Port = tomlInt(race, "socks5_outbound_port")
	}
	if t := tomlStrList(hoard, "proxy_v2_trusted_sources"); len(t) > 0 {
		f.ProxyV2Trusted = t
	} else {
		f.ProxyV2Trusted = tomlStrList(race, "proxy_v2_trusted_sources")
	}
	return f
}

// gluetunEngineFromTOML reports "race" only when the race section is the one
// following the forwarded port. Everything else, including a config that
// predates the choice, reads as hoard.
func gluetunEngineFromTOML(race map[string]interface{}) string {
	if tomlBool(race, "gluetun_port_forward") {
		return "race"
	}
	return "hoard"
}

func quotedList(items []string) string {
	parts := make([]string, 0, len(items))
	for _, it := range items {
		if s := strings.TrimSpace(it); s != "" {
			parts = append(parts, strconv.Quote(s))
		}
	}
	return "[" + strings.Join(parts, ", ") + "]"
}

// validateNetMode refuses the setups that cannot work, with the reason in
// words. Every one of these has a failure mode that looks like something else
// entirely once it is running.
func validateNetMode(mode string, f netModeFields) error {
	port := func(name string, p int) error {
		if p < 1 || p > 65535 {
			return fmt.Errorf("%s must be between 1 and 65535", name)
		}
		return nil
	}
	if err := port("the race listen port", f.RaceListenPort); err != nil {
		return err
	}
	if err := port("the hoard listen port", f.HoardListenPort); err != nil {
		return err
	}
	if f.RaceListenPort == f.HoardListenPort {
		return fmt.Errorf("the two engines cannot share listen port %d: the second one to start would fail to bind", f.RaceListenPort)
	}
	// An interface that does not exist is refused in EVERY mode that can carry
	// one. Accepting it would leave the engine on the default route at runtime,
	// which is the failure the binding exists to prevent and the one nothing
	// downstream can report.
	for _, scope := range []string{"race", "hoard"} {
		name := bindInterfaceFor(f, scope)
		if name == "" {
			continue
		}
		if _, err := net.InterfaceByName(name); err != nil {
			return fmt.Errorf("no interface named %q on this host for the %s engine: its sockets would fall back to the default route", name, scope)
		}
	}
	switch mode {
	case netModeDirect:
	case netModeGluetun:
		if bindInterfaceFor(f, "race") == "" && bindInterfaceFor(f, "hoard") == "" {
			return fmt.Errorf("gluetun mode needs the interface name the tunnel creates (wg0, tun0, …) on at least one engine, otherwise nothing is inside the tunnel")
		}
		if f.GluetunPort {
			switch strings.TrimSpace(f.GluetunEngine) {
			case "", "race", "hoard":
			default:
				return fmt.Errorf("the forwarded port goes to the race or the hoard engine, not %q", f.GluetunEngine)
			}
		}
	case netModeSocks5, netModeProxyV2:
		if strings.TrimSpace(f.Socks5Host) == "" {
			return fmt.Errorf("this mode needs the SOCKS5 proxy address")
		}
		if err := port("the SOCKS5 port", f.Socks5Port); err != nil {
			return err
		}
		if (f.Socks5User == "") != (f.Socks5Pass == "") {
			return fmt.Errorf("give both a SOCKS5 username and password, or neither")
		}
		if mode == netModeProxyV2 {
			if f.RaceProxyV2Port == 0 && f.HoardProxyV2Port == 0 {
				return fmt.Errorf("PROXY-v2 mode needs at least one PROXY-v2 listen port")
			}
			for name, p := range map[string]int{"the race PROXY-v2 port": f.RaceProxyV2Port, "the hoard PROXY-v2 port": f.HoardProxyV2Port} {
				if p == 0 {
					continue
				}
				if err := port(name, p); err != nil {
					return err
				}
				if p == f.RaceListenPort || p == f.HoardListenPort {
					return fmt.Errorf("%s (%d) is already a plain listen port", name, p)
				}
			}
			if f.RaceProxyV2Port != 0 && f.RaceProxyV2Port == f.HoardProxyV2Port {
				return fmt.Errorf("the two engines cannot share PROXY-v2 port %d", f.RaceProxyV2Port)
			}
			if len(f.ProxyV2Trusted) == 0 {
				return fmt.Errorf("list the addresses allowed to send PROXY-v2 headers: with none, anyone reaching that port can claim to be any peer")
			}
			for _, src := range f.ProxyV2Trusted {
				src = strings.TrimSpace(src)
				if net.ParseIP(src) == nil {
					if _, _, err := net.ParseCIDR(src); err != nil {
						return fmt.Errorf("trusted source %q is neither an IP address nor a CIDR range", src)
					}
				}
			}
		}
	default:
		return fmt.Errorf("unknown network mode %q", mode)
	}
	return nil
}

// netModeKeys renders the TOML keys one engine gets for the chosen mode. Every
// mode returns the SAME key set — the ones it does not use are blanked, never
// omitted — so switching modes cannot leave a stale key behind.
func netModeKeys(mode string, f netModeFields, listenPort, proxyV2Port int, scope string) [][2]string {
	socksHost, socksPort := "", 0
	socksUser, socksPass := "", ""
	announceProxy := ""
	bindIface := ""
	pv2Addr, pv2Trusted := "", []string(nil)

	// Direct and gluetun both carry the binding, and each engine gets its own.
	// The proxy modes blank it: there the egress decision is the proxy's, and a
	// leftover interface would pin the hop that reaches it to a tunnel that the
	// operator no longer believes is in play.
	if mode == netModeDirect || mode == netModeGluetun {
		bindIface = bindInterfaceFor(f, scope)
	}
	switch mode {
	case netModeSocks5, netModeProxyV2:
		socksHost, socksPort = strings.TrimSpace(f.Socks5Host), f.Socks5Port
		socksUser, socksPass = f.Socks5User, f.Socks5Pass
		// One proxy entered once, wired to BOTH paths. Peer dials read the
		// socks5_outbound_* keys inside the engine; announces read
		// announce_proxy on the Go side. Setting only the first is what made a
		// relay hide the traffic while the tracker still recorded the operator's
		// own address, so the tab never lets them come apart.
		u := &url.URL{Scheme: "socks5h", Host: net.JoinHostPort(socksHost, strconv.Itoa(socksPort))}
		if socksUser != "" {
			u.User = url.UserPassword(socksUser, socksPass)
		}
		announceProxy = u.String()
		if mode == netModeProxyV2 {
			pv2Addr, pv2Trusted = strings.TrimSpace(f.ProxyV2Addr), f.ProxyV2Trusted
			if pv2Addr == "" {
				pv2Addr = "0.0.0.0"
			}
		}
	}
	if mode != netModeProxyV2 {
		proxyV2Port = 0
	}
	gluetunOn, gluetunURL, gluetunKey := false, "", ""
	// Exactly one engine follows the forwarded port, and the operator says
	// which: a provider hands out one port, so pointing both at it would have
	// the second fail to bind. The other scope gets the flag written false on
	// the same save, so switching engines can never leave both of them on it.
	if mode == netModeGluetun && f.GluetunPort && scope == gluetunEngineOf(f) {
		gluetunOn = true
		gluetunURL = strings.TrimSpace(f.GluetunURL)
		gluetunKey = f.GluetunAPIKey
	}
	return [][2]string{
		{"gluetun_port_forward", strconv.FormatBool(gluetunOn)},
		{"gluetun_url", strconv.Quote(gluetunURL)},
		{"gluetun_api_key", strconv.Quote(gluetunKey)},
		{"listen_port", strconv.Itoa(listenPort)},
		{"enable_ipv6", strconv.FormatBool(f.EnableIPv6)},
		{"bind_interface", strconv.Quote(bindIface)},
		{"socks5_outbound_host", strconv.Quote(socksHost)},
		{"socks5_outbound_port", strconv.Itoa(socksPort)},
		{"socks5_outbound_user", strconv.Quote(socksUser)},
		{"socks5_outbound_pass", strconv.Quote(socksPass)},
		{"announce_proxy", strconv.Quote(announceProxy)},
		{"listen_port_proxy_v2", strconv.Itoa(proxyV2Port)},
		{"listen_addr_proxy_v2", strconv.Quote(pv2Addr)},
		{"proxy_v2_trusted_sources", quotedList(pv2Trusted)},
	}
}

// applyEgressToSession stamps a mode's egress decision onto one engine's
// config struct. It exists for the extra engines: those live in engines.json as
// a FROZEN COPY of their role's primary, taken once at creation time, so every
// later save on this page rewrote [race]/[hoard] and left the shards pointed at
// the old tunnel. Nothing failed, and the page showed the new interface -- the
// shard just kept announcing from the old one. Same silent class as the
// announce leak, one level down.
//
// Only EGRESS fields are touched. listen_port belongs to the shard (they must
// differ or the second one fails to bind), and so does its id and role.
//
// Kept in step with netModeKeys by TestExtraEnginesGetTheSameEgressAsTheirRole,
// which compares the two field by field: two writers of the same decision drift
// the moment someone adds a key to one of them.
func applyEgressToSession(mode string, f netModeFields, role string, sc *config.SessionConfig) {
	sc.BindInterface = ""
	if mode == netModeDirect || mode == netModeGluetun {
		sc.BindInterface = bindInterfaceFor(f, role)
	}
	sc.EnableIPv6 = f.EnableIPv6
	sc.Socks5OutboundHost, sc.Socks5OutboundPort = "", 0
	sc.Socks5OutboundUser, sc.Socks5OutboundPass = "", ""
	sc.AnnounceProxy = ""
	if mode == netModeSocks5 || mode == netModeProxyV2 {
		sc.Socks5OutboundHost, sc.Socks5OutboundPort = strings.TrimSpace(f.Socks5Host), f.Socks5Port
		sc.Socks5OutboundUser, sc.Socks5OutboundPass = f.Socks5User, f.Socks5Pass
		u := &url.URL{Scheme: "socks5h", Host: net.JoinHostPort(sc.Socks5OutboundHost, strconv.Itoa(sc.Socks5OutboundPort))}
		if sc.Socks5OutboundUser != "" {
			u.User = url.UserPassword(sc.Socks5OutboundUser, sc.Socks5OutboundPass)
		}
		sc.AnnounceProxy = u.String()
	}
}

// syncExtraEnginesEgress carries this page's decision to every extra engine.
// A shard follows its ROLE: a race shard leaves by whatever the race engine
// leaves by. Giving a shard a tunnel of its own is a bigger feature; silently
// leaving it on last month's tunnel is a bug, and that is what this fixes.
func (s *Server) syncExtraEnginesEgress(mode string, f netModeFields) (int, error) {
	dataDir := s.config.Daemon.DataDir
	extras, err := config.LoadExtraEngines(dataDir)
	if err != nil || len(extras) == 0 {
		return 0, err
	}
	for i := range extras {
		applyEgressToSession(mode, f, extras[i].Role, &extras[i].SessionConfig)
	}
	if err := config.SaveExtraEngines(dataDir, extras); err != nil {
		return 0, err
	}
	return len(extras), nil
}

func (s *Server) handleNetworkModePost(c *gin.Context) {
	var req struct {
		Mode   string        `json:"mode"`
		Fields netModeFields `json:"fields"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if err := validateNetMode(req.Mode, req.Fields); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}

	configWriteMu.Lock()
	defer configWriteMu.Unlock()
	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	doc := string(data)
	for _, e := range []struct {
		section     string
		listenPort  int
		proxyV2Port int
	}{
		{"race", req.Fields.RaceListenPort, req.Fields.RaceProxyV2Port},
		{"hoard", req.Fields.HoardListenPort, req.Fields.HoardProxyV2Port},
	} {
		doc, err = config.SetTOMLTable(doc, e.section, netModeKeys(req.Mode, req.Fields, e.listenPort, e.proxyV2Port, e.section))
		if err != nil {
			c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
			return
		}
	}
	if _, err := config.ParseTOMLMap([]byte(doc)); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "edited config no longer parses: " + err.Error()})
		return
	}
	if err := config.ValidateTyped([]byte(doc)); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "edit would break the config schema: " + err.Error()})
		return
	}
	_ = os.WriteFile(path+".bak-network", data, 0644)
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(doc), 0644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if err := os.Rename(tmp, path); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	warnings := modeWarnings(req.Mode, req.Fields, collectEnvOverrides())
	// The extra engines are written AFTER the TOML is safely in place: if this
	// fails the primaries are already correct, and the shards are reported as
	// stale rather than the whole save being rolled back into an unknown state.
	synced, serr := s.syncExtraEnginesEgress(req.Mode, req.Fields)
	if serr != nil {
		warnings = append(warnings, "The extra engines could not be updated ("+serr.Error()+"), so they keep their previous network settings. Until that is fixed they may announce from a different address than the one shown here.")
	}
	c.JSON(http.StatusOK, gin.H{
		"mode":                  req.Mode,
		"restart_required":      true,
		"agents_notified":       s.pushConfigToAgentsAsync(),
		"extra_engines_updated": synced,
		"warnings":              warnings,
	})
}

// netCheckResult is one line of the report. Status is ok / warn / fail, and
// "detail" carries the measurement itself so a screenshot of this page is
// enough to diagnose a setup remotely.
type netCheckResult struct {
	ID     string `json:"id"`
	Label  string `json:"label"`
	Status string `json:"status"`
	Detail string `json:"detail"`
}

func (s *Server) handleNetworkCheck(c *gin.Context) {
	var req struct {
		EchoURL string `json:"echo_url"`
	}
	_ = c.ShouldBindJSON(&req)
	echo := strings.TrimSpace(req.EchoURL)

	m, err := s.readNetworkConfig()
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	race, hoard := sectionOf(m, "race"), sectionOf(m, "hoard")
	mode := detectNetMode(race, hoard)

	ctx, cancel := context.WithTimeout(c.Request.Context(), 25*time.Second)
	defer cancel()

	results := make([]netCheckResult, 0, 6)
	add := func(id, label, status, detail string) {
		results = append(results, netCheckResult{ID: id, Label: label, Status: status, Detail: detail})
	}

	// 1. What a tracker sees. Measured through the announce client itself, for
	// each engine separately: they carry independent settings and have already
	// been caught disagreeing.
	announceIPs := map[string]string{}
	for _, e := range []struct {
		name string
		sec  map[string]interface{}
	}{{"race", race}, {"hoard", hoard}} {
		b := engine.ApplyAnnounceEgress(
			engine.DefaultSingleBinding(tomlInt(e.sec, "listen_port"), tomlBool(e.sec, "enable_ipv6"), e.name, 0),
			tomlStr(e.sec, "announce_proxy"), tomlStr(e.sec, "announce_ip"),
			tomlStr(e.sec, "socks5_outbound_host"), tomlStr(e.sec, "bind_interface"), e.name)[0]
		ip, err := engine.AnnounceEgressIP(ctx, b, echo)
		if err != nil {
			add("announce_"+e.name, "Address trackers see ("+e.name+")", "fail", err.Error())
			continue
		}
		announceIPs[e.name] = ip
		add("announce_"+e.name, "Address trackers see ("+e.name+")", "ok", ip)
	}

	// 2. What a peer sees, PER ENGINE. It used to be measured on the hoard
	// alone and labelled as though it spoke for both, which held only while a
	// single shared interface was the one thing this tab could express. With a
	// tunnel per engine, a hoard-only reading reports the hoard's address as the
	// race engine's and hides exactly the split this feature makes possible.
	peerIPs, peerErrs := map[string]string{}, map[string]error{}
	for _, e := range []struct {
		name string
		sec  map[string]interface{}
	}{{"race", race}, {"hoard", hoard}} {
		ip, err := engine.PeerEgressIP(ctx,
			tomlStr(e.sec, "socks5_outbound_host"), tomlInt(e.sec, "socks5_outbound_port"),
			tomlStr(e.sec, "socks5_outbound_user"), tomlStr(e.sec, "socks5_outbound_pass"),
			tomlStr(e.sec, "bind_interface"), echo)
		peerErrs[e.name] = err
		label := "Address peers see (" + e.name + ")"
		if err != nil {
			detail := err.Error()
			if bi := tomlStr(e.sec, "bind_interface"); bi != "" {
				detail += ". Peer connections are bound to \"" + bi + "\""
				if !looksLikeTunnel(bi) {
					detail += ", which does not look like a VPN tunnel: that is the usual reason they go nowhere"
				}
			}
			add("peer_egress_"+e.name, label, "fail", detail)
			continue
		}
		peerIPs[e.name] = ip
		add("peer_egress_"+e.name, label, "ok", ip)
	}

	// 3. Where the daemon's other requests go. NOT "the host's own address":
	// inside a VPN or proxy namespace this leaves by the tunnel like everything
	// else, so it cannot serve as an outside reference. Informational only.
	hostIP := getPublicIP()
	if hostIP == "" {
		add("host_ip", "Address the daemon's own requests use", "warn", "could not be determined")
	} else {
		add("host_ip", "Address the daemon's own requests use", "ok", hostIP)
	}

	// 4. The comparison that actually catches the defect: do the two paths
	// agree? The reported leak was peers relayed and announces direct, which
	// shows up here as two different addresses. Comparing against the daemon's
	// own address instead would call a working VPN a leak, since inside the
	// tunnel every path shares one address, and that is correct rather than
	// suspicious.
	// The gate is now PER ENGINE and keys on that engine having an egress of its
	// own, rather than on the mode. Under the old rule the whole comparison was
	// skipped in direct mode -- which is exactly where a per-engine interface
	// now lives, so the one setup that most needs the check would have been the
	// one setup never checked.
	compared := false
	for _, e := range []struct {
		name string
		sec  map[string]interface{}
	}{{"race", race}, {"hoard", hoard}} {
		if tomlStr(e.sec, "bind_interface") == "" &&
			tomlStr(e.sec, "socks5_outbound_host") == "" && tomlStr(e.sec, "announce_proxy") == "" {
			continue // nothing configured to leave by: both paths are the WAN, and agreeing proves nothing
		}
		compared = true
		ip, ok := announceIPs[e.name]
		if !ok {
			continue
		}
		switch {
		case peerErrs[e.name] != nil:
			add("paths_"+e.name, "Announce path ("+e.name+")", "warn",
				"trackers see "+ip+", but the peer path could not be measured, so the two cannot be compared")
		case ip != peerIPs[e.name]:
			add("paths_"+e.name, "Announce path ("+e.name+")", "fail",
				"trackers see "+ip+" while peers are dialled from "+peerIPs[e.name]+". The address that identifies you is the one the tracker records")
		default:
			add("paths_"+e.name, "Announce path ("+e.name+")", "ok",
				"announces and peer connections both leave from "+ip)
		}
	}
	// Two engines pinned to two interfaces SHOULD disagree with each other.
	// Said out loud, because a page that has spent years reporting a mismatch as
	// a leak would otherwise teach the reader to distrust a correct setup.
	if a, b := announceIPs["race"], announceIPs["hoard"]; a != "" && b != "" && a != b {
		add("engines_differ", "The two engines", "ok",
			"race announces from "+a+" and hoard from "+b+". Two different addresses is what a per-engine interface is for, not a fault")
	}
	if compared {
		add("scope", "Scope of this check", "warn",
			"measured from inside this daemon's own network namespace: it can tell the announce path from the peer path, but an address that exists outside both is invisible to it")
	}

	for _, w := range modeWarnings(mode, netFieldsFromTOML(race, hoard), collectEnvOverrides()) {
		add("note", "Worth knowing", "warn", w)
	}

	// Inbound: knock on the very address and port a tracker hands out for us,
	// leaving by the peer route. Through a proxy or tunnel that connection
	// genuinely comes from outside. On a direct setup it goes out and back over
	// the same WAN address, which tests the router's hairpin rather than the
	// outside world, so the verdict says which of the two was measured.
	for _, e := range []struct {
		name string
		sec  map[string]interface{}
	}{{"race", race}, {"hoard", hoard}} {
		ip, port := announceIPs[e.name], tomlInt(e.sec, "listen_port")
		label := "Inbound reachability (" + e.name + ")"
		if ip == "" || port == 0 {
			add("inbound_"+e.name, label, "warn", "not tested: the announced address could not be measured")
			continue
		}
		target := net.JoinHostPort(ip, strconv.Itoa(port))
		// Name a torrent this engine holds, so the far end can be made to prove
		// it is us rather than merely accepting the socket.
		sample := ""
		if e.name == "race" && s.raceEngine != nil {
			sample = s.raceEngine.SampleServedInfoHash()
		} else if e.name == "hoard" && s.hoardEngine != nil {
			sample = s.hoardEngine.SampleServedInfoHash()
		}
		peerID, err := engine.InboundReachable(ctx, ip, port,
			tomlStr(e.sec, "socks5_outbound_host"), tomlInt(e.sec, "socks5_outbound_port"),
			tomlStr(e.sec, "socks5_outbound_user"), tomlStr(e.sec, "socks5_outbound_pass"),
			tomlStr(e.sec, "bind_interface"), sample)
		// Only a probe leaving through a proxy reaches us the way a stranger
		// would. Through a tunnel it turns around at the VPN provider, and on a
		// direct setup at your own router, and neither is obliged to loop it
		// back. Success proves reachability in all three; failure only proves
		// it in the first.
		viaProxy := tomlStr(e.sec, "socks5_outbound_host") != ""
		turnaround := "your own router"
		if mode == netModeGluetun || tomlStr(e.sec, "bind_interface") != "" {
			turnaround = "your VPN provider"
		}
		switch {
		case err == nil && viaProxy:
			add("inbound_"+e.name, label, "ok", "your client answered on "+target+" (peer "+peerID+") to a connection coming from outside your network: peers can reach you")
		case err == nil:
			add("inbound_"+e.name, label, "ok", "your client answered on "+target+" (peer "+peerID+"): the port is open")
		case errors.Is(err, engine.ErrNotUs) && sample == "":
			add("inbound_"+e.name, label, "warn", target+" accepted the connection, but this engine holds no torrent to prove with, so there is no telling whether the answer came from your client")
		case errors.Is(err, engine.ErrNotUs):
			add("inbound_"+e.name, label, "fail", target+" accepted the connection but did not answer as your client, so peers are not reaching you. Beware that some VPN providers accept every port from inside their own tunnel, forwarded or not, which looks exactly like this")
		case mode == netModeSocks5:
			add("inbound_"+e.name, label, "fail", target+" refused the connection, as expected: a plain SOCKS5 proxy forwards outgoing connections only, so nobody can reach you")
		case viaProxy:
			add("inbound_"+e.name, label, "fail", target+" refused a connection coming from outside: peers cannot reach you. Check the port forward and the firewall. ("+err.Error()+")")
		default:
			add("inbound_"+e.name, label, "warn", target+" refused the connection, but the probe had to turn around at "+turnaround+", which is not obliged to send it back to you. Inconclusive rather than closed. ("+err.Error()+")")
		}
	}

	// Extra engines last, and only when there are any. They are shards created
	// from the Agents menu, they carry their own copy of the egress settings in
	// engines.json, and until now nothing on this page could see them: a shard
	// left on a stale tunnel announced from the wrong address while every line
	// above reported green. Measured through the same announce client as the
	// primaries, so this says what the shard DOES, not what its config claims.
	s.addExtraEngineChecks(ctx, echo, race, hoard, add)

	c.JSON(http.StatusOK, gin.H{"mode": mode, "results": results})
}

// addExtraEngineChecks measures every UI-managed extra engine and compares it
// with the primary of its role, which is the address the page displays.
func (s *Server) addExtraEngineChecks(ctx context.Context, echo string,
	race, hoard map[string]interface{}, add func(id, label, status, detail string)) {

	extras, err := config.LoadExtraEngines(s.config.Daemon.DataDir)
	if err != nil {
		add("extra_engines", "Extra engines", "warn", "could not be read ("+err.Error()+"), so their network settings are unverified")
		return
	}
	if len(extras) == 0 {
		return
	}
	for _, e := range extras {
		sec := hoard
		if e.Role == "race" {
			sec = race
		}
		label := "Extra engine " + e.ID + " (" + e.Role + ")"
		// The config drift is worth naming on its own, before any measurement:
		// it is the thing that goes wrong silently, and it is readable without
		// touching the network.
		if want := tomlStr(sec, "bind_interface"); want != e.BindInterface {
			add("extra_drift_"+e.ID, label, "fail",
				"leaves by "+quoteOrDefaultRoute(e.BindInterface)+" while the "+e.Role+
					" engine leaves by "+quoteOrDefaultRoute(want)+
					". Save the network settings again to bring it back in step")
		}
		b := engine.ApplyAnnounceEgress(
			engine.DefaultSingleBinding(e.ListenPort, e.EnableIPv6, e.Role, 0),
			e.AnnounceProxy, e.AnnounceIP, e.Socks5OutboundHost, e.BindInterface, e.Role)[0]
		ip, aerr := engine.AnnounceEgressIP(ctx, b, echo)
		if aerr != nil {
			add("extra_announce_"+e.ID, label, "fail", "announce path: "+aerr.Error())
			continue
		}
		add("extra_announce_"+e.ID, label, "ok", "announces from "+ip)
	}
}

// quoteOrDefaultRoute names an interface, or says plainly what an empty one
// means. Printing "" in a warning about egress is how an operator reads "not
// set" as "fine".
func quoteOrDefaultRoute(name string) string {
	if strings.TrimSpace(name) == "" {
		return "the host's default route"
	}
	return strconv.Quote(name)
}
