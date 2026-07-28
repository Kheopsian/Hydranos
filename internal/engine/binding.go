package engine

import (
	"math/rand"

	"github.com/Kheopsian/hydra/internal/version"
)

// Binding describes one network identity through which Hydra announces and
// dials peers. With multiple bindings, each one has its own peer_id, source
// IP for outbound traffic, listen port, and tracker-visible public IP — the
// goal being to bypass per-source-IP transport caps (e.g. Free QoS, Proton
// per-tunnel ceilings) by spreading the swarm across multiple WireGuard
// tunnels that each look like a separate BitTorrent client.
//
// Single-binding (the legacy setup) just instantiates one Binding with
// empty ListenAddr/PublicIP so the kernel picks the default route.
type Binding struct {
	// ID is a stable index used in logs and metrics. 0 = primary binding.
	ID int
	// PeerID is the 20-byte BitTorrent peer_id used in tracker announces and
	// peer handshakes. Each binding announces with a distinct peer_id so
	// trackers see N separate clients in the swarm.
	PeerID string
	// ListenAddr is the local IP this binding listens on. Multi-tunnel
	// Proton setup shares one Address ("10.2.0.2") across all bindings —
	// per-tunnel routing is by Fwmark, not src IP.
	ListenAddr string
	// ListenPort is the BitTorrent listen port for inbound peers on THIS
	// binding. Distinct per binding even when ListenAddr is shared, since
	// NAT-PMP forwards (server_public_IP, external_port) → (10.2.0.2, this
	// port) differently per tunnel.
	ListenPort int
	// AnnouncePort is the publicly-reachable port to announce to trackers
	// and advertise in the BEP-10 extension handshake. Differs from
	// ListenPort when behind NAT (e.g. Proton WG: ListenPort = local
	// internal port, AnnouncePort = NAT-PMP-mapped external port). 0 means
	// "use ListenPort" — correct for the legacy single-binding setup where
	// no NAT translation is in play.
	AnnouncePort int
	// PublicIP is the IP we advertise to trackers via the BEP-7 `&ip=` param.
	// For Proton WG, it's the tunnel's exit public IP (e.g. "146.70.194.114").
	// Empty = omit the param and let the tracker observe the source IP itself
	// (correct behavior for the legacy single-binding setup).
	PublicIP string
	// Fwmark is the netfilter mark applied to outbound sockets bound to
	// this binding so the kernel routes them through the right WG tunnel
	// (matched by `ip rule fwmark X lookup tableX`). 0 = no fwmark
	// (legacy single-tunnel via FOU/wstunnel — kernel default route is fine).
	Fwmark uint32
}

// DefaultSingleBinding returns the legacy single-binding slice for the given
// listen port. PeerID is generated from the version's fingerprint (random
// suffix). Fwmark=0 (kernel default route). Used by main.go before the
// multi-tunnel setup is wired up so existing single-tunnel deployments
// behave unchanged.
func DefaultSingleBinding(listenPort int) []Binding {
	return []Binding{{
		ID:         0,
		PeerID:     generatePeerID(version.PeerFingerprint()),
		ListenAddr: "",
		ListenPort: listenPort,
		PublicIP:   "",
		Fwmark:     0,
	}}
}

// generatePeerID builds a 20-byte BEP-20 peer_id: 8-byte prefix
// (e.g. "-HY244i-") followed by 12 random alphanumeric bytes.
func generatePeerID(prefix string) string {
	const chars = "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
	suffix := make([]byte, 12)
	for i := range suffix {
		suffix[i] = chars[rand.Intn(len(chars))]
	}
	return prefix + string(suffix)
}
