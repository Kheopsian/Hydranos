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

// crossProbeAttempts counts the connections this node made to its OWN engines
// while probing them from another engine.
//
// Without it the two halves of this file would confirm each other: the cross
// probe opens a real peer connection to the target, the target counts it in
// InboundAccepted, and the next pass reads that counter and reports "someone
// connected to you" -- naming our own probe as the visitor. A green dot backed
// by nothing but itself is worse than the honest "unknown" it replaced, so the
// passive evidence below only counts arrivals BEYOND the ones we caused.
var (
	crossProbeMu       sync.Mutex
	crossProbeAttempts = map[string]int64{}
)

func noteCrossProbe(name string) {
	crossProbeMu.Lock()
	crossProbeAttempts[name]++
	crossProbeMu.Unlock()
}

func crossProbesFor(name string) int64 {
	crossProbeMu.Lock()
	defer crossProbeMu.Unlock()
	return crossProbeAttempts[name]
}

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

// proberFor picks another engine of this node that can dial `t` from a
// genuinely different place on the network.
//
// "Different" means a different MEASURED exit address. Two engines behind one
// tunnel share the provider's turnaround, so one probing the other learns
// exactly what the self-probe already fails to learn: the provider is under no
// obligation to send a connection back to its own client. With no such peer the
// caller keeps the previous evidence chain rather than inventing a verdict.
//
// This is the one vantage point a single-homed node cannot buy: a second engine
// on a second tunnel IS somebody else as far as the first engine's port
// forwarding is concerned.
func proberFor(t reachTarget, peers []reachTarget) *reachTarget {
	if t.PublicIP == "" {
		return nil
	}
	for i := range peers {
		p := &peers[i]
		if p.Name == t.Name || p.PublicIP == "" || p.PublicIP == t.PublicIP {
			continue
		}
		return p
	}
	return nil
}

// crossProbePort is the port a real peer would dial, and whether we actually
// KNOW that: true only when the provider told us its mapping.
//
// The distinction decides what a failure is allowed to mean. Behind a VPN doing
// NAT-PMP, the engine's listen port is the internal side of a mapping whose
// external port is chosen by the provider and is not the same number. Dialling
// the internal one from outside reaches a port nobody forwards, so it fails
// whatever the state of the real forwarding -- and reporting "unreachable" from
// that is a confident lie about a node that may well be reachable.
func crossProbePort(t reachTarget) (int, bool) {
	if gport, _, active := GluetunStatus(t.Name); active && gport > 0 {
		return gport, true
	}
	return t.Port, false
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

func (s *Server) probeReachability(ctx context.Context, t reachTarget, peers []reachTarget) {
	name, port := t.Name, t.Port
	ip := t.PublicIP
	if ip == "" {
		ip = getPublicIP()
	}
	if ip == "" || port == 0 {
		setReachability(name, reachState{State: "unknown", Detail: "no public address or listen port yet", At: time.Now()})
		return
	}

	// Set when a cross-engine probe ran and failed. Its verdict is applied
	// after the passive check, never before it.
	crossTried := false
	crossDetail := ""
	// Set when a cross probe failed on a port we were not sure about. It
	// becomes the detail of an "unknown", never of a verdict.
	crossUnknown := ""

	// Strongest evidence first, when this node can produce it: another engine
	// leaving by another tunnel dials this one's forwarded port and is answered
	// by its peer_id. That is a real peer arriving from elsewhere, which is the
	// exact thing the self-probe cannot be.
	if prober := proberFor(t, peers); prober != nil {
		if sample := targetSample(t); sample != "" {
			noteCrossProbe(name)
			dialPort, portKnown := crossProbePort(t)
			peerID, err := engine.InboundReachable(ctx, ip, dialPort,
				prober.SocksHost, prober.SocksPort, prober.SocksUser, prober.SocksPass,
				prober.Iface, sample)
			if err == nil {
				setReachability(name, reachState{
					State: "reachable",
					Detail: fmt.Sprintf("answered as %s, dialled on port %d from %s (exit %s)",
						peerID, dialPort, prober.Name, prober.PublicIP),
					At: time.Now(),
				})
				return
			}
			// This failure means something the self-probe's never did: the
			// dial genuinely came from another exit address, so our own
			// tunnel's turnaround cannot explain it. Held rather than acted on
			// straight away -- a peer that actually arrived outranks a probe
			// that did not get through, so the passive check gets to speak
			// first.
			crossTried = portKnown
			crossDetail = fmt.Sprintf("%s (exit %s) could not reach port %d: %v",
				prober.Name, prober.PublicIP, dialPort, err)
			if !portKnown {
				// We dialled the engine's own listen port because nothing
				// told us the forwarded one. Behind a provider that maps a
				// different external port, that dial was always going to
				// fail, so it says nothing about the forwarding. Left for
				// the evidence chain below rather than turned into a red
				// dot the operator cannot act on.
				crossUnknown = fmt.Sprintf(
					"%s (exit %s) could not reach port %d, but that is this agent's own listen port: no forwarded port is known for it, so nothing here says whether peers can get in",
					prober.Name, prober.PublicIP, dialPort)
			}
		}
	}

	// A peer that reached us settles the question outright, in every mode,
	// including inside a tunnel where our own probe is blind. Arrivals we
	// caused ourselves are discounted: see crossProbeAttempts.
	if t.Inbound != nil {
		if n, err := t.Inbound(); err == nil {
			if mine := crossProbesFor(name); n > mine {
				setReachability(name, reachState{
					State:  "reachable",
					Detail: fmt.Sprintf("%d peers have connected to you since start", n-mine),
					At:     time.Now(),
				})
				return
			}
		}
	}

	if crossTried {
		setReachability(name, reachState{State: "unreachable", Detail: crossDetail, At: time.Now()})
		return
	}
	if crossUnknown != "" {
		setReachability(name, reachState{State: "unknown", Detail: crossUnknown, At: time.Now()})
		return
	}

	sample := targetSample(t)
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

// targetSample names a torrent the engine serves, which is what turns a bare
// connect into proof: a provider accepts every port from inside its own tunnel,
// only a completed handshake says the answer came from us.
func targetSample(t reachTarget) string {
	if t.Sample == nil {
		return ""
	}
	return t.Sample()
}

func (s *Server) probeReachabilityOnce() {
	targets := s.reachTargets()
	for _, t := range targets {
		// One deadline per engine, not one for the pass: a node with five
		// engines would otherwise have the last of them share what the first
		// left, and a probe that times out is indistinguishable from a port
		// that is closed.
		ctx, cancel := context.WithTimeout(context.Background(), reachProbeTimeout)
		s.probeReachability(ctx, t, targets)
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
		// The RESOLVED engine, not a composed push: the composer zeroes the
		// keys that belong to the node, and this probe is the node.
		ec, ok := resolvedEngine(cfg, id)
		if !ok {
			return
		}
		out = append(out, reachTarget{
			Name: key, Port: port, PublicIP: exitOf(LocalAgentNameFor(id)),
			Iface:     ec.BindInterface,
			SocksHost: ec.Socks5OutboundHost, SocksPort: ec.Socks5OutboundPort,
			SocksUser: ec.Socks5OutboundUser, SocksPass: ec.Socks5OutboundPass,
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
