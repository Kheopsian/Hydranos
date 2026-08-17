package api

import (
	"context"
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
func (s *Server) probeReachability(ctx context.Context, name string, sec map[string]interface{}) {
	port := tomlInt(sec, "listen_port")
	ip := getPublicIP()
	if ip == "" || port == 0 {
		setReachability(name, reachState{State: "unknown", Detail: "no public address or listen port yet", At: time.Now()})
		return
	}
	var sample string
	if name == "race" && s.raceEngine != nil {
		sample = s.raceEngine.SampleServedInfoHash()
	} else if name == "hoard" && s.hoardEngine != nil {
		sample = s.hoardEngine.SampleServedInfoHash()
	}
	if sample == "" {
		// With no torrent to name, a handshake cannot be demanded, and a bare
		// connect is not evidence: a VPN provider accepts every port from
		// inside its own tunnel. Unknown is the honest answer.
		setReachability(name, reachState{State: "unknown", Detail: "no torrent loaded to prove the answer came from us", At: time.Now()})
		return
	}
	peerID, err := engine.InboundReachable(ctx, ip, port,
		tomlStr(sec, "socks5_outbound_host"), tomlInt(sec, "socks5_outbound_port"),
		tomlStr(sec, "socks5_outbound_user"), tomlStr(sec, "socks5_outbound_pass"),
		tomlStr(sec, "bind_interface"), sample)
	if err != nil {
		// A failure only means "closed" when the probe genuinely came from
		// outside, which is the case through a proxy. Inside a tunnel it goes
		// out and comes back to the provider's own address, and a provider is
		// under no obligation to send it back to its own client: measured on
		// ProtonVPN, a port that a peer reaches perfectly well answers nothing
		// from within the tunnel. On a direct setup the same turnaround happens
		// at the router. Calling either of those unreachable would have the dot
		// contradict a node that works.
		viaProxy := strings.TrimSpace(tomlStr(sec, "socks5_outbound_host")) != ""
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
	m, err := s.readNetworkConfig()
	if err != nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), reachProbeTimeout)
	defer cancel()
	s.probeReachability(ctx, "race", sectionOf(m, "race"))
	s.probeReachability(ctx, "hoard", sectionOf(m, "hoard"))
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
