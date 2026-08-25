package engine

import (
	"log/slog"
	"math/rand"
	"os"
	"strings"

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
	// EnableIPv6 lets this binding take the IPv6 peers a tracker returns in
	// the BEP-7 `peers6` field. Off by default, mirroring the engine setting:
	// without a v6 listener those peers are of no use to us, and dialling
	// them would advertise a return path we cannot serve.
	EnableIPv6 bool
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
	// AnnounceScope names the engine this binding announces for ("hoard",
	// "race"). Together with ID it keys the shared announce rate limiter, so
	// every announcer of one engine+binding draws from the same bucket.
	AnnounceScope string
	// AnnounceRateLimit caps outbound announces on this binding, in announces
	// per second. 0 = unlimited. Mirrors the engine's announce_rate_limit.
	AnnounceRateLimit float64
	// AnnounceProxy is the SOCKS5 URL every announce on this binding is sent
	// through ("socks5h://user:pass@host:port"). Empty falls back to the
	// process-wide TYPHON_ANNOUNCE_PROXY env, then to a direct announce.
	AnnounceProxy string
	// BindInterface is the NAME of the interface every socket of this binding
	// must leave by ("wg0", "tun0", "eth1"). Resolved to its current IPv4 at
	// announcer build time rather than stored as an IP, so it survives the
	// tunnel address rotating on reconnect.
	//
	// It exists on the binding, and not only in the engine config, because the
	// engine and the announce path are two different processes: Typhon binds
	// its peer sockets from the Rust side, and the tracker announce is dialled
	// here in Go. Setting it in one place only produced peers inside the tunnel
	// and announces outside it, which no green indicator can show.
	BindInterface string
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
// behave unchanged. `enableIPv6` comes from the engine's own setting, so the
// announcer only takes v6 peers for an engine that actually listens on v6.
// `scope` + `announceRateLimit` carry the engine's announce_rate_limit; 0 keeps
// announces unthrottled.
func DefaultSingleBinding(listenPort int, enableIPv6 bool, scope string, announceRateLimit float64) []Binding {
	return []Binding{{
		ID:                0,
		PeerID:            generatePeerID(version.PeerFingerprint()),
		ListenAddr:        "",
		ListenPort:        listenPort,
		PublicIP:          "",
		Fwmark:            0,
		EnableIPv6:        enableIPv6,
		AnnounceScope:     scope,
		AnnounceRateLimit: announceRateLimit,
	}}
}

// ApplyAnnounceEgress stamps a session's announce-egress settings onto every
// one of its bindings, and shouts when they contradict the peer-dial settings.
//
// bindInterface is stamped here for the same reason the proxy is: it is an
// egress decision, and the announce path has to make the same one the engine
// makes or the tracker records an address the peers never use.
//
// The warning is the point of this function as much as the wiring is. The two
// proxies are independent by design — one for peer dials in the engine, one for
// announces here — and an operator who configures only the first gets a working
// relay whose transport is hidden while the tracker still records the host's own
// address. Nothing fails, nothing is logged, and the setup looks correct from
// every angle they can check. So say it out loud at startup instead.
func ApplyAnnounceEgress(bs []Binding, announceProxy, announceIP, socks5OutboundHost, bindInterface, scope string) []Binding {
	proxy := strings.TrimSpace(announceProxy)
	ip := strings.TrimSpace(announceIP)
	iface := strings.TrimSpace(bindInterface)
	for i := range bs {
		bs[i].AnnounceProxy = proxy
		bs[i].BindInterface = iface
		if ip != "" {
			bs[i].PublicIP = ip
		}
	}
	if proxy == "" && strings.TrimSpace(os.Getenv("TYPHON_ANNOUNCE_PROXY")) == "" &&
		strings.TrimSpace(socks5OutboundHost) != "" {
		slog.Warn("announce egress: peer dials go through the SOCKS5 proxy but announces do NOT, "+
			"so the tracker records this host's own address. Set announce_proxy in this session's "+
			"config to send them through the proxy too.",
			"engine", scope, "socks5_outbound_host", socks5OutboundHost)
	}
	return bs
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
