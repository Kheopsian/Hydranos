package api

import (
	"context"
	"net/http"
	"sort"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/agentwire"
	"github.com/Kheopsian/hydra/internal/engine"
	"github.com/gin-gonic/gin"
)

// echoURLv6 is the AAAA-only echo, so a v6 measurement cannot silently answer
// over v4 and report an address the engine never announces.
const echoURLv6 = "https://api6.ipify.org/"

// What each engine of this node actually does on the network.
//
// The header used to show one exit IP and two dots. Both were true when a node
// was two engines behind one route; neither survived per-engine interfaces. The
// address in particular was never measured per engine at all: getPublicIP()
// asks an echo service FROM THE PROCESS, so it reports the default route --
// give three engines three tunnels and all three lines show the same address,
// and an engine leaking outside its tunnel looks exactly like one inside it.
//
// So it is measured the way the Network tab's check measures it: through each
// engine's own binding, which is what its announces and its peers use.

// engineNet is one engine's network identity, as the header and its panel show
// it.
type engineNet struct {
	Agent    string `json:"agent"`
	Engine   string `json:"engine"`
	Role     string `json:"role"`
	Iface    string `json:"bind_interface,omitempty"`
	Port     int    `json:"listen_port,omitempty"`
	ExitIP   string `json:"exit_ip,omitempty"`
	ExitIPv6 string `json:"exit_ip_v6,omitempty"`
	// State is ok | warn | bad | off, matching the dot colours: reachable,
	// not established yet, unreachable, or the node is down.
	State  string `json:"state"`
	Detail string `json:"detail,omitempty"`
	Local  bool   `json:"local"`
}

// engineNetInterval is how often the exit addresses are re-measured. Each pass
// costs one request per engine per family, out through that engine's own path,
// so this is a background timer and never a per-request measurement.
const engineNetInterval = 3 * time.Minute

// engineNetAttempt bounds ONE measurement. Two attempts per family per engine,
// so the whole pass stays well inside its own budget however many engines a
// node runs.
const engineNetAttempt = 6 * time.Second

var (
	engineNetMu   sync.RWMutex
	engineNetRows []engineNet
	engineNetAt   time.Time
)

// EngineNetSnapshot returns the last measurement. Empty until the first pass:
// the header shows nothing rather than something it has not checked.
func EngineNetSnapshot() ([]engineNet, time.Time) {
	engineNetMu.RLock()
	defer engineNetMu.RUnlock()
	out := make([]engineNet, len(engineNetRows))
	copy(out, engineNetRows)
	return out, engineNetAt
}

func setEngineNet(rows []engineNet) {
	engineNetMu.Lock()
	engineNetRows, engineNetAt = rows, time.Now()
	engineNetMu.Unlock()
}

// localEngineRows lists the engines this process runs: the two primaries plus
// every extra one. Config for each comes from the same composer the config push
// uses, so what is measured is what the engine was told to be.
func (s *Server) localEngineRows() []engineNet {
	if s.frontOnly {
		return nil
	}
	cfg := s.liveConfig()
	var rows []engineNet
	add := func(id, role string, port int) {
		sess, err := cfg.ComposeSession(LocalAgentNameFor(id), id, role)
		if err != nil {
			return
		}
		iface := sess.BindInterface
		rows = append(rows, engineNet{
			Agent: LocalAgentNameFor(id), Engine: id, Role: role,
			Iface: iface, Port: port, Local: true, State: "warn",
			Detail: "not measured yet",
		})
	}
	if s.raceEngine != nil {
		add(agentwire.EngineRace, "race", s.raceEngine.ListenPort())
	}
	if s.hoardEngine != nil {
		add(agentwire.EngineHoard, "hoard", s.hoardEngine.ListenPort())
	}
	if s.engineHost != nil {
		for _, e := range s.engineHost.Engines() {
			add(e.ID, e.Role, e.ListenPort)
		}
	}
	return rows
}

// measureEngineNet fills in each local engine's exit addresses and state.
func (s *Server) measureEngineNet(ctx context.Context) {
	rows := s.localEngineRows()
	cfg := s.liveConfig()
	// Last known addresses, kept when a measurement fails.
	prevRows, _ := EngineNetSnapshot()
	previous := make(map[string]engineNet, len(prevRows))
	for _, p := range prevRows {
		if p.ExitIP != "" {
			previous[p.Agent] = p
		}
	}
	for i := range rows {
		if i > 0 {
			// Spread the passes: three engines resolving the same name through
			// the same tunnel in the same instant is how the timeouts started.
			select {
			case <-ctx.Done():
			case <-time.After(300 * time.Millisecond):
			}
		}
		r := &rows[i]
		sess, err := cfg.ComposeSession(r.Agent, r.Engine, r.Role)
		if err != nil {
			r.State, r.Detail = "off", err.Error()
			continue
		}
		b := engine.ApplyAnnounceEgress(
			engine.DefaultSingleBinding(r.Port, sess.EnableIPv6, r.Role, 0),
			sess.AnnounceProxy, sess.AnnounceIP, sess.Socks5OutboundHost, r.Iface, r.Role)[0]
		exitErr := ""
		if ip, aErr := measureExit(ctx, b, ""); aErr == nil {
			r.ExitIP = ip
		} else {
			exitErr = "exit address unknown: " + aErr.Error()
		}
		if sess.EnableIPv6 {
			if ip6, aErr := measureExit(ctx, b, echoURLv6); aErr == nil {
				r.ExitIPv6 = ip6
			}
		}
		// Keep what was measured before rather than blanking the line. A DNS
		// lookup that leaves through a tunnel times out often enough that a
		// strict reading would empty the panel every few passes, and an address
		// from three minutes ago is far more useful than none -- the panel says
		// when it was taken.
		if r.ExitIP == "" {
			if prev, ok := previous[r.Agent]; ok {
				r.ExitIP, r.ExitIPv6 = prev.ExitIP, prev.ExitIPv6
			}
		}
		// Reachability: the primaries have a probe already. The extra engines
		// have none yet, and saying "reachable" for something nobody tested is
		// the one answer that misleads -- so they stay amber until they have
		// their own probe.
		switch r.Engine {
		case agentwire.EngineRace, agentwire.EngineHoard:
			st := reachabilityOf(r.Role)
			// The address failure is kept in front of the reachability detail,
			// never overwritten by it: an engine whose exit could not be
			// measured is the more urgent of the two things to say.
			r.Detail = joinDetail(exitErr, st.Detail)
			switch st.State {
			case "reachable":
				r.State = "ok"
			case "unreachable":
				r.State = "bad"
			default:
				r.State = "warn"
			}
		default:
			r.State = "warn"
			r.Detail = joinDetail(exitErr, "no reachability probe for extra engines yet")
		}
	}
	// Agents on other machines, as they already report themselves: one exit
	// address per NODE, which is what their NodeInfo answers. Measuring theirs
	// per engine needs the probe to run on their side; until then this says
	// what is actually known rather than repeating this host's address.
	for _, ra := range s.agentsSnapshot() {
		if ra.local {
			continue
		}
		info, err := s.agentNodeInfo(ra)
		for _, e := range ra.engines {
			row := engineNet{Agent: ra.name, Engine: e.id, Role: e.role, State: "off"}
			if err == nil {
				row.ExitIP, row.ExitIPv6 = info.PublicIP, info.PublicIPv6
				row.State = "ok"
			} else {
				row.Detail = err.Error()
			}
			rows = append(rows, row)
		}
	}
	setEngineNet(rows)
}

// measuredAtUnix reports 0 for "never", not the year 1 in seconds -- which is
// what a zero time marshals to, and what a client would render as a date.
func measuredAtUnix(t time.Time) int64 {
	if t.IsZero() {
		return 0
	}
	return t.Unix()
}

// joinDetail puts the more urgent sentence first and drops the empty ones.
func joinDetail(parts ...string) string {
	out := parts[:0]
	for _, p := range parts {
		if strings.TrimSpace(p) != "" {
			out = append(out, p)
		}
	}
	return strings.Join(out, " · ")
}

// measureExit asks the echo service through one engine's binding, once more if
// the first attempt fails.
//
// The retry is not politeness. Every engine measured here resolves the echo's
// name through its own tunnel, several of them within the same second, and a
// resolver behind a VPN drops one of those often enough that a single attempt
// reports "unknown" for an engine that is working perfectly.
func measureExit(parent context.Context, b engine.Binding, echoURL string) (string, error) {
	// A deadline per ATTEMPT, not one shared by the whole pass. With a single
	// budget the first engine to sit on a dead IPv6 route spent it all and the
	// engines after it were reported unmeasured -- the measurement starving the
	// thing it was measuring.
	try := func() (string, error) {
		ctx, cancel := context.WithTimeout(parent, engineNetAttempt)
		defer cancel()
		return engine.AnnounceEgressIP(ctx, b, echoURL)
	}
	ip, err := try()
	if err == nil {
		return ip, nil
	}
	select {
	case <-parent.Done():
		return "", err
	case <-time.After(700 * time.Millisecond):
	}
	return try()
}

// agentNodeInfo asks one agent for its exit address, tolerating a node that is
// simply down.
func (s *Server) agentNodeInfo(ra *remoteAgent) (agentwire.NodeInfo, error) {
	cl := ra.anyClient()
	if cl == nil {
		return agentwire.NodeInfo{}, errAgentUnreachable
	}
	return cl.NodeInfo()
}

var errAgentUnreachable = errorString("agent is not reachable")

type errorString string

func (e errorString) Error() string { return string(e) }

// StartEngineNetProbe measures every engine's egress on a timer. Delayed like
// the reachability probe: at boot the engines are still loading and a tunnel
// may not be up, and an amber dot for a node that is merely not ready reads as
// a fault.
func (s *Server) StartEngineNetProbe() {
	go func() {
		time.Sleep(20 * time.Second)
		for {
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
			s.measureEngineNet(ctx)
			cancel()
			time.Sleep(engineNetInterval)
		}
	}()
}

// handleEngineNet serves the last measurement. It never measures: the header
// polls it, and a probe per poll would open one connection per engine per
// browser tab.
var engineNetOnce sync.Once

func (s *Server) handleEngineNet(c *gin.Context) {
	// Started on first use, like the reachability probe: no wiring in main, and
	// a node whose header is never opened pays nothing.
	engineNetOnce.Do(s.StartEngineNetProbe)
	// A manual refresh asks for a new measurement and does not wait for it: N
	// engines times two families of request is seconds of round trips, and the
	// click that asked for it is a header button.
	if c.Query("refresh") != "" {
		go func() {
			ctx, cancel := context.WithTimeout(context.Background(), 3*time.Minute)
			defer cancel()
			s.measureEngineNet(ctx)
		}()
	}
	rows, at := EngineNetSnapshot()
	if rows == nil {
		rows = []engineNet{}
	}
	// Distinct exit addresses decide what the header can honestly show: one
	// address means it can print it, several mean it cannot.
	seen := map[string]bool{}
	for _, r := range rows {
		if r.Local && r.ExitIP != "" {
			seen[r.ExitIP] = true
		}
	}
	exits := make([]string, 0, len(seen))
	for ip := range seen {
		exits = append(exits, ip)
	}
	sort.Strings(exits)
	var v6 string
	if len(exits) == 1 {
		for _, r := range rows {
			if r.Local && r.ExitIP == exits[0] && r.ExitIPv6 != "" {
				v6 = r.ExitIPv6
				break
			}
		}
	}
	c.JSON(http.StatusOK, gin.H{
		"engines":     rows,
		"exits":       exits,
		"exit_ip_v6":  v6,
		"measured_at": measuredAtUnix(at),
	})
}
