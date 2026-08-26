package api

import (
	"context"
	"fmt"
	"strings"
	"sync"
	"time"

	"github.com/Kheopsian/hydra/internal/engine"
)

// The header dot used to report `peers > 0`, which answers a different question
// than the one it appears to answer: peers we dialled ourselves prove nothing
// about anyone being able to reach us. A node that nobody can connect to looks
// perfectly healthy by that measure, and stays a leech-only node forever.
//
// So the dot is backed by the same probe the Network tab uses: connect to the
// address a tracker publishes for us and complete a BitTorrent handshake, which
// only our own client can answer.
//
// It runs on a timer rather than per request. The probe opens a real connection
// and the header polls every few seconds; tying the two together would mean
// hammering our own address forever.
const (
	reachProbeInterval = 10 * time.Minute
	reachProbeTimeout  = 20 * time.Second
)

type reachState struct {
	State  string    `json:"state"` // "reachable" | "unreachable" | "unknown"
	Detail string    `json:"detail,omitempty"`
	At     time.Time `json:"at,omitempty"`
}

var (
	reachProbeOnce sync.Once
	reachMu        sync.RWMutex
	reachByName    = map[string]reachState{}
)

func reachabilityOf(name string) reachState {
	reachMu.RLock()
	defer reachMu.RUnlock()
	if st, ok := reachByName[name]; ok {
		return st
	}
	return reachState{State: "unknown"}
}

func setReachability(name string, st reachState) {
	reachMu.Lock()
	reachByName[name] = st
	reachMu.Unlock()
}

// probeReachability tests one engine and records the verdict.
//
// It aims at getPublicIP() rather than re-measuring the announce address on
// every pass: that measurement costs its own request to an outside service, and
// the two agree in every mode except a SOCKS5 setup whose orchestrator proxy
// differs from its announce proxy. The Network tab keeps the rigorous version,
// which asks the announce client itself; this one has to stay cheap enough to
// run unattended.
// reachTarget is one engine to test: where it listens, how it leaves, and how
// to ask it whether anyone has already arrived.
//
// It exists because this probe named "race" and "hoard" in six places. Those
// two are engines like any other since 3.138.0, and an engine added from the
// Agents page had no probe at all -- its vertex in the header stayed amber
// saying so, which is honest but useless.
type reachTarget struct {
	// Name is the key the state is stored under: the ROLE for the two
	// primaries, because /api/port-forward reads them that way, and the engine
	// id for everything else.
	Name      string
	Port      int
	PublicIP  string
	Iface     string
	SocksHost string
	SocksPort int
	SocksUser string
	SocksPass string
	// Inbound and Sample come from the engine itself; nil where the engine
	// cannot be reached from here, which turns the probe into "unknown" rather
	// than a guess.
	Inbound func() (int64, error)
	Sample  func() string
}

func (s *Server) probeReachability(ctx context.Context, t reachTarget) {
	name, port := t.Name, t.Port
	ip := t.PublicIP
	if ip == "" {
		ip = getPublicIP()
	}
	if ip == "" || port == 0 {
		setReachability(name, reachState{State: "unknown", Detail: "no public address or listen port yet", At: time.Now()})
		return
	}
	// Checked first: a peer that reached us settles the question outright, in
	// every mode, including inside a tunnel where our own probe is blind.
	if t.Inbound != nil {
		if n, err := t.Inbound(); err == nil && n > 0 {
			setReachability(name, reachState{
				State:  "reachable",
				Detail: fmt.Sprintf("%d peers have connected to you since start", n),
				At:     time.Now(),
			})
			return
		}
	}

	var sample string
	if t.Sample != nil {
		sample = t.Sample()
	}
	if sample == "" {
		// With no torrent to name, a handshake cannot be demanded, and a bare
		// connect is not evidence: a VPN provider accepts every port from
		// inside its own tunnel. Unknown is the honest answer.
		setReachability(name, reachState{State: "unknown", Detail: "no torrent loaded to prove the answer came from us", At: time.Now()})
		return
	}
	peerID, err := engine.InboundReachable(ctx, ip, port,
		t.SocksHost, t.SocksPort, t.SocksUser, t.SocksPass, t.Iface, sample)
	if err != nil {
		// A failure only means "closed" when the probe genuinely came from
		// outside, which is the case through a proxy. Inside a tunnel it goes
		// out and comes back to the provider's own address, and a provider is
		// under no obligation to send it back to its own client: measured on
		// ProtonVPN, a port that a peer reaches perfectly well answers nothing
		// from within the tunnel. On a direct setup the same turnaround happens
		// at the router. Calling either of those unreachable would have the dot
		// contradict a node that works.
		viaProxy := strings.TrimSpace(t.SocksHost) != ""
		if viaProxy {
			setReachability(name, reachState{State: "unreachable", Detail: err.Error(), At: time.Now()})
			return
		}
		detail := "not established: this probe leaves through your own tunnel or router and comes back to the same address, which is not obliged to return it. A peer coming from elsewhere may well get through"
		if gport, _, active := GluetunStatus(name); active && gport == port {
			detail += ". Gluetun reports this port as forwarded"
		}
		setReachability(name, reachState{State: "unknown", Detail: detail, At: time.Now()})
		return
	}
	setReachability(name, reachState{State: "reachable", Detail: "answered as " + peerID, At: time.Now()})
}

func (s *Server) probeReachabilityOnce() {
	for _, t := range s.reachTargets() {
		// One deadline per engine, not one for the pass: a node with five
		// engines would otherwise have the last of them share what the first
		// left, and a probe that times out is indistinguishable from a port
		// that is closed.
		ctx, cancel := context.WithTimeout(context.Background(), reachProbeTimeout)
		s.probeReachability(ctx, t)
		cancel()
	}
}

// reachTargets lists every engine of this node with what the probe needs.
//
// Each one is measured at ITS OWN exit address when that has been measured
// (enginenet.go), not at the process's default route. On a node whose engines
// leave by different tunnels those are different addresses, and probing the
// wrong one answers a question about somebody else's port.
func (s *Server) reachTargets() []reachTarget {
	cfg := s.liveConfig()
	rows, _ := EngineNetSnapshot()
	exitOf := func(agent string) string {
		for _, r := range rows {
			if r.Agent == agent {
				return r.ExitIP
			}
		}
		return ""
	}
	var out []reachTarget
	add := func(key, id, role string, port int, inbound func() (int64, error), sample func() string) {
		sess, err := cfg.ComposeSession(LocalAgentNameFor(id), id, role)
		if err != nil {
			return
		}
		out = append(out, reachTarget{
			Name: key, Port: port, PublicIP: exitOf(LocalAgentNameFor(id)),
			Iface:     sess.BindInterface,
			SocksHost: sess.Socks5OutboundHost, SocksPort: sess.Socks5OutboundPort,
			SocksUser: sess.Socks5OutboundUser, SocksPass: sess.Socks5OutboundPass,
			Inbound: inbound, Sample: sample,
		})
	}
	if s.raceEngine != nil {
		add("race", "race", "race", s.raceEngine.ListenPort(),
			s.raceEngine.InboundAccepted, s.raceEngine.SampleServedInfoHash)
	}
	if s.hoardEngine != nil {
		add("hoard", "hoard", "hoard", s.hoardEngine.ListenPort(),
			s.hoardEngine.InboundAccepted, s.hoardEngine.SampleServedInfoHash)
	}
	if s.engineHost != nil {
		for _, e := range s.engineHost.Engines() {
			id := e.ID
			add(id, id, e.Role, e.ListenPort,
				func() (int64, error) { return s.engineHost.InboundAccepted(id) },
				func() string { return s.engineHost.SampleServedInfoHash(id) })
		}
	}
	return out
}

// StartReachabilityProbe runs the check in the background for as long as the
// daemon lives. The first pass is delayed: at boot nothing has announced, the
// engines are still loading their torrents, and a probe then would report
// unreachable for a node that is merely not ready.
func (s *Server) StartReachabilityProbe() {
	go func() {
		time.Sleep(90 * time.Second)
		for {
			s.probeReachabilityOnce()
			time.Sleep(reachProbeInterval)
		}
	}()
}
