package engine

import (
	"bytes"
	"context"
	"crypto/tls"
	"fmt"
	"github.com/Kheopsian/hydra/internal/config"
	"github.com/Kheopsian/hydra/internal/version"
	"io"
	"log/slog"
	"math/rand"
	"net"
	"net/http"
	"net/url"
	"os"
	"sort"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"time"
)

// TrackerPeer is a peer returned by a tracker announce.
type TrackerPeer struct {
	IP   string
	Port int
}

// TrackerAnnounceResult holds the parsed tracker response.
type TrackerAnnounceResult struct {
	Peers         []TrackerPeer
	Interval      int
	Complete      int
	Incomplete    int
	FailureReason string
}

// trackerAnnouncer performs direct HTTP announces to trackers,
// bypassing libtorrent's announce scheduler. One announcer per Binding —
// each carries its own peer_id, public IP (advertised via BEP-7 &ip=),
// and source IP (HTTP client source-bound to that local addr so the
// announce request leaves through the right WireGuard tunnel).
// A dial that never returns costs far more than the announce it was carrying.
// net/http purges Transport.dialsInProgress only from the front, so a single
// hung dial pins every wantConn queued behind it, and each of those retains
// the connectMethod built from the announce URL. With DisableKeepAlives every
// announce dials, so the queue grows for as long as the head stays stuck: a
// tracker that accepts the TCP connection and then never finishes the TLS
// handshake left 36M live wantConn (~9 GB of heap) behind three pinned dials
// after 29 h at 196k torrents. The Client Timeout does not cover this -- the
// dial is detached from the request, so cancelling the request never unblocks
// it. Only the Transport's own timeouts do, and a Transport literal
// zero-values them (means "no limit"); http.DefaultTransport is what sets
// them, and none of these Transports inherit from it.
const (
	announceTLSHandshakeTimeout   = 10 * time.Second
	announceResponseHeaderTimeout = 20 * time.Second
)

type trackerAnnouncer struct {
	httpClient      *http.Client
	secondaryClient *http.Client // optional, fire-and-forget via TYPHON_ANNOUNCE_V6_PROXY (v4 egress path)
	// clientV4/clientV6 are the same transport pinned to one address family.
	// A tracker that records only the announce source address holds whichever
	// family we happened to leave by, so a dual-stack host has to announce on
	// both to be reachable by both — this is what libtorrent does, one peer_id
	// with two addresses. Nil when an announce proxy is configured (the egress
	// family is then the proxy's) or when the host lacks that family.
	clientV4  *http.Client
	clientV6  *http.Client
	peerID    string
	port      int
	publicIP  string
	userAgent string
	bindingID int
	livePort  *atomic.Int64 // runtime port override (nil/0 = use static port)
	fwmark    int           // SO_MARK for this binding's egress; also used by the udp:// path
	// proxied reports that announces leave through a SOCKS5 proxy. The udp://
	// path reads it to refuse rather than fall back to a direct datagram.
	proxied    bool
	enableIPv6 bool // take the tracker's BEP-7 `peers6` list
	// limiter throttles outbound announces (announce_rate_limit). Shared per
	// engine+binding, nil when the setting is 0 (unlimited).
	limiter *announceLimiter
	// gate holds every announce while the engine is in its startup pause.
	// Shared per engine scope, never nil (an unheld gate is a no-op).
	gate *startupGate
}

// newTrackerAnnouncerForBinding builds an announcer wired to a specific
// Binding. Per-tunnel routing is via Fwmark (multi-tunnel Proton); legacy
// single-binding leaves Fwmark=0 and uses the kernel default route.
//
// Source-bind via LocalAddr is no longer used — multi-tunnel Proton has all
// bindings share Address=10.2.0.2 so src-IP can't disambiguate. Fwmark
// + `ip rule fwmark X lookup tableX` does the steering instead.
// ipv4Network narrows a dial network to its IPv4 form. Anything it does not
// recognise is passed through untouched rather than guessed at.
func ipv4Network(network string) string {
	switch network {
	case "tcp", "tcp6":
		return "tcp4"
	case "udp", "udp6":
		return "udp4"
	}
	return network
}

// ipv6Network narrows a dial network to its IPv6 form. Mirror of ipv4Network:
// anything it does not recognise is passed through untouched.
func ipv6Network(network string) string {
	switch network {
	case "tcp", "tcp4":
		return "tcp6"
	case "udp", "udp4":
		return "udp6"
	}
	return network
}

// HostHasIPv4 reports whether this host holds a usable IPv4 address, ignoring
// loopback. Callers use it to say at boot that an announce pinned to IPv4 has
// nowhere to go, rather than letting every announce fail on its own later.
func HostHasIPv4() bool {
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		return true // cannot tell: do not raise a false alarm
	}
	for _, a := range addrs {
		ipn, ok := a.(*net.IPNet)
		if !ok || ipn.IP.IsLoopback() {
			continue
		}
		if ipn.IP.To4() != nil {
			return true
		}
	}
	return false
}

// HostHasIPv6 reports whether this host holds a globally routable IPv6 address.
// Link-local and loopback do not count: a tracker cannot be reached from them,
// so treating them as v6 connectivity would make every companion announce fail.
func HostHasIPv6() bool {
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		return false // cannot tell: do not fire announces that will fail
	}
	for _, a := range addrs {
		ipn, ok := a.(*net.IPNet)
		if !ok {
			continue
		}
		if ipn.IP.To4() != nil {
			continue
		}
		if ipn.IP.IsGlobalUnicast() && !ipn.IP.IsLinkLocalUnicast() {
			return true
		}
	}
	return false
}

func newTrackerAnnouncerForBinding(b Binding) *trackerAnnouncer {
	dialer := &net.Dialer{
		Timeout:   5 * time.Second,
		KeepAlive: -1,
	}
	applyFwmark(dialer, int(b.Fwmark))

	// enable_ipv6 governed the listener, the tracker's peers6 list and the
	// self-dial filter, but never this: a plain "tcp" dial follows RFC 6724 and
	// prefers IPv6 wherever the host has it, so the announce left over v6 and
	// the tracker recorded a v6 address for someone whose setting says IPv4
	// only. Pin the family instead of hoping the host has no v6.
	//
	// A configured SOCKS proxy is deliberately left alone below: the egress
	// family is then the proxy's, and forcing v4 to reach it would break a
	// proxy that is only reachable over v6.
	// A per-tracker override wins over the binding-wide setting: a host may be
	// dual-stack everywhere and still need one tracker pinned to v4, because
	// trackers that record only the announce SOURCE ip then hold an address
	// v4-only peers cannot dial. addr here is the tracker host:port, so the
	// family is decided per request rather than per client.
	base := dialer
	dial := func(ctx context.Context, network, addr string) (net.Conn, error) {
		switch announceIPModeFor(addr) {
		case AnnounceIPModeV4:
			return base.DialContext(ctx, ipv4Network(network), addr)
		case AnnounceIPModeV6:
			return base.DialContext(ctx, ipv6Network(network), addr)
		}
		if !b.EnableIPv6 {
			return base.DialContext(ctx, ipv4Network(network), addr)
		}
		return base.DialContext(ctx, network, addr)
	}

	transport := &http.Transport{
		TLSClientConfig:       &tls.Config{InsecureSkipVerify: true},
		DisableKeepAlives:     true,
		DialContext:           dial,
		TLSHandshakeTimeout:   announceTLSHandshakeTimeout,
		ResponseHeaderTimeout: announceResponseHeaderTimeout,
	}
	// If a SOCKS5 outbound proxy is configured (TYPHON_ANNOUNCE_PROXY env),
	// route every announce through it. The base dialer above is reused to
	// reach the proxy itself (so SO_MARK still applies if the proxy lives
	// behind a fwmark-routed tunnel — though typical setup is the proxy at
	// a globally-reachable v6 addr that the kernel routes via the default
	// table, in which case Fwmark=0 and no marking happens).
	announceProxy := announceProxyForBinding(b)
	if pu := announceProxy; pu != nil {
		socksAddr := pu.Host
		socksUser := pu.User.Username()
		socksPass, _ := pu.User.Password()
		baseDialer := dialer
		transport.DialContext = func(ctx context.Context, network, addr string) (net.Conn, error) {
			host, portStr, err := net.SplitHostPort(addr)
			if err != nil {
				return nil, err
			}
			portN, err := strconv.Atoi(portStr)
			if err != nil {
				return nil, err
			}
			return dialSOCKS5h(ctx, baseDialer, socksAddr, socksUser, socksPass, host, uint16(portN))
		}
	}
	// Family-pinned clients for the dual-family announce. Only built on a direct
	// egress: behind a SOCKS proxy the tracker sees the proxy's address, so
	// pinning the local family changes nothing and would only break a proxy
	// reachable over one family. Skipped for a family this host does not have,
	// so we never fire an announce that cannot connect.
	var clientV4, clientV6 *http.Client
	if announceProxy == nil {
		pinned := func(narrow func(string) string) *http.Client {
			base := dialer
			return &http.Client{Timeout: 10 * time.Second, Transport: &http.Transport{
				TLSClientConfig:   &tls.Config{InsecureSkipVerify: true},
				DisableKeepAlives: true,
				DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
					return base.DialContext(ctx, narrow(network), addr)
				},
				TLSHandshakeTimeout:   announceTLSHandshakeTimeout,
				ResponseHeaderTimeout: announceResponseHeaderTimeout,
			}}
		}
		if HostHasIPv4() {
			clientV4 = pinned(ipv4Network)
		}
		if HostHasIPv6() {
			clientV6 = pinned(ipv6Network)
		}
	}

	// Optional secondary client routed through TYPHON_ANNOUNCE_V6_PROXY.
	// Used to fire a parallel announce so the tracker registers us via a
	// different egress path (typically v4 SNAT, when primary egresses v6).
	// Without this, CloudFlare-fronted private trackers only see our v6
	// source and v4-only peers can't reach us.
	var secondaryClient *http.Client
	if pu := loadSecondaryAnnounceProxy(); pu != nil {
		socksAddr := pu.Host
		socksUser := pu.User.Username()
		socksPass, _ := pu.User.Password()
		baseDialer := dialer
		secondaryTransport := &http.Transport{
			TLSClientConfig:       &tls.Config{InsecureSkipVerify: true},
			DisableKeepAlives:     true,
			TLSHandshakeTimeout:   announceTLSHandshakeTimeout,
			ResponseHeaderTimeout: announceResponseHeaderTimeout,
			DialContext: func(ctx context.Context, network, addr string) (net.Conn, error) {
				host, portStr, err := net.SplitHostPort(addr)
				if err != nil {
					return nil, err
				}
				portN, err := strconv.Atoi(portStr)
				if err != nil {
					return nil, err
				}
				return dialSOCKS5h(ctx, baseDialer, socksAddr, socksUser, socksPass, host, uint16(portN))
			},
		}
		secondaryClient = &http.Client{Timeout: 15 * time.Second, Transport: secondaryTransport}
	}

	peerID := b.PeerID
	if peerID == "" {
		peerID = generateQBitPeerID()
	}
	// Tracker port = AnnouncePort if non-zero (NAT-PMP path), else ListenPort
	// (legacy direct-listen path with no NAT translation).
	port := b.ListenPort
	if b.AnnouncePort > 0 {
		port = b.AnnouncePort
	}
	return &trackerAnnouncer{
		httpClient:      &http.Client{Timeout: 10 * time.Second, Transport: transport},
		secondaryClient: secondaryClient,
		clientV4:        clientV4,
		clientV6:        clientV6,
		peerID:          peerID,
		port:            port,
		publicIP:        b.PublicIP,
		userAgent:       version.UserAgent(),
		bindingID:       b.ID,
		fwmark:          int(b.Fwmark),
		proxied:         announceProxy != nil,
		enableIPv6:      b.EnableIPv6,
		limiter:         announceLimiterFor(b.AnnounceScope+"#"+strconv.Itoa(b.ID), b.AnnounceRateLimit),
		gate:            startupGateFor(b.AnnounceScope),
	}
}

// announceProxyForBinding resolves the SOCKS5 proxy this binding's announces go
// through: the per-session announce_proxy first, else the process-wide env.
// Returns nil for a direct announce. A malformed value is refused rather than
// guessed at — but it cannot silently become a direct announce without saying
// so, since that is exactly the leak the setting exists to prevent.
func announceProxyForBinding(b Binding) *url.URL {
	raw := strings.TrimSpace(b.AnnounceProxy)
	if raw == "" {
		return loadAnnounceProxy()
	}
	u, err := url.Parse(raw)
	if err != nil || (u.Scheme != "socks5" && u.Scheme != "socks5h") {
		slog.Warn("tracker_announce: announce_proxy ignored (parse failure or unsupported scheme) "+
			"announces go DIRECT and the tracker will record this host's own address",
			"engine", b.AnnounceScope, "binding", b.ID, "error", err)
		return nil
	}
	slog.Info("tracker_announce: announces routed through the session's SOCKS5 proxy",
		"engine", b.AnnounceScope, "binding", b.ID, "host", u.Host, "auth", u.User.Username() != "")
	return u
}

// announceProxyOnce caches the parsed TYPHON_ANNOUNCE_PROXY env at startup.
// Format expected: socks5h://user:pass@host:port — sets a SOCKS5h dialer for
// HTTP transports built via newTrackerAnnouncerForBinding so all tracker
// announces sortent par le proxy. Used to route through a remote SOCKS5
// (e.g. microsocks on a VPS reachable over IPv6) bypassing local-network
// DPI shaping on UDP/TCP bulk paths.
var (
	announceProxyOnce sync.Once
	announceProxyURL  *url.URL
)

// Per-tracker announce passkey overrides (host-substring -> passkey). Lets an
// instance announce under another account without re-fetching .torrents. Seeded
// from config [announce_passkeys] + legacy env TYPHON_ANNOUNCE_PASSKEY (torr9),
// hot-swappable at runtime. Empty = dormant (announce verbatim from the torrent).
var (
	passkeyOverrideMu sync.RWMutex
	passkeyOverrides  = map[string]string{}
)

// InitPasskeyOverrides seeds the override map from config + the legacy env.
func InitPasskeyOverrides(fromConfig map[string]string) {
	passkeyOverrideMu.Lock()
	defer passkeyOverrideMu.Unlock()
	for host, pk := range fromConfig {
		if h, p := strings.TrimSpace(host), strings.TrimSpace(pk); h != "" && p != "" {
			passkeyOverrides[h] = p
		}
	}
	if env := strings.TrimSpace(os.Getenv("TYPHON_ANNOUNCE_PASSKEY")); env != "" {
		passkeyOverrides["tracker.torr9.net"] = env
	}
	if len(passkeyOverrides) > 0 {
		slog.Info("tracker_announce: passkey overrides active", "trackers", len(passkeyOverrides))
	}
}

// ResetPasskeyOverrides REPLACES the override map with the given one.
//
// The Init* seeders above merge, which is right for a boot that reads one
// config file. It is wrong for an agent taking a whole configuration pushed by
// its front: there, an override the operator deleted on the front is expressed
// as an absence, and merging would leave the agent announcing under a passkey
// nobody can see any more.
func ResetPasskeyOverrides(fromFront map[string]string) {
	passkeyOverrideMu.Lock()
	clear(passkeyOverrides)
	passkeyOverrideMu.Unlock()
	InitPasskeyOverrides(fromFront)
}

// SetPasskeyOverride sets (passkey=="" clears) the announce passkey for trackers
// whose URL contains host. Takes effect on the next announce (no restart).
func SetPasskeyOverride(host, passkey string) {
	host = strings.TrimSpace(host)
	if host == "" {
		return
	}
	passkeyOverrideMu.Lock()
	defer passkeyOverrideMu.Unlock()
	if strings.TrimSpace(passkey) == "" {
		delete(passkeyOverrides, host)
	} else {
		passkeyOverrides[host] = strings.TrimSpace(passkey)
	}
}

// GetPasskeyOverrides returns a copy of the current override map.
func GetPasskeyOverrides() map[string]string {
	passkeyOverrideMu.RLock()
	defer passkeyOverrideMu.RUnlock()
	out := make(map[string]string, len(passkeyOverrides))
	for k, v := range passkeyOverrides {
		out[k] = v
	}
	return out
}

// ClientSpoof is a fake BitTorrent client identity presented to a specific
// tracker to pass its client whitelist (e.g. MAM rejects our -HY…- peer_id
// with "Non-Whitelisted client"). PeerIDPrefix replaces the 8-byte BEP-20
// prefix of the announce peer_id (suffix kept stable so the tracker sees a
// consistent peer across announces); UserAgent overrides the HTTP header.
type ClientSpoof struct {
	PeerIDPrefix string // 8 chars, e.g. "-qB5220-"
	UserAgent    string // e.g. "qBittorrent/5.2.2"
}

// Per-tracker client spoof overrides (host-substring -> ClientSpoof). Seeded
// from config [announce_clients], hot-swappable via POST /api/announce/clients.
// Empty = dormant (announce with our real -HY…- identity).
var (
	clientOverrideMu sync.RWMutex
	clientOverrides  = map[string]ClientSpoof{}
)

// InitClientOverrides seeds the spoof map from config at boot.
func InitClientOverrides(fromConfig map[string]ClientSpoof) {
	clientOverrideMu.Lock()
	defer clientOverrideMu.Unlock()
	for host, c := range fromConfig {
		if h := strings.TrimSpace(host); h != "" && strings.TrimSpace(c.PeerIDPrefix) != "" {
			clientOverrides[h] = ClientSpoof{
				PeerIDPrefix: strings.TrimSpace(c.PeerIDPrefix),
				UserAgent:    strings.TrimSpace(c.UserAgent),
			}
		}
	}
	if len(clientOverrides) > 0 {
		slog.Info("tracker_announce: client spoof overrides active", "trackers", len(clientOverrides))
	}
}

// ResetClientOverrides REPLACES the spoof map (see ResetPasskeyOverrides).
func ResetClientOverrides(fromFront map[string]ClientSpoof) {
	clientOverrideMu.Lock()
	clear(clientOverrides)
	clientOverrideMu.Unlock()
	InitClientOverrides(fromFront)
}

// SetClientOverride sets (peerIDPrefix=="" clears) the spoof for trackers whose
// URL contains host. Takes effect on the next announce (no restart).
func SetClientOverride(host, peerIDPrefix, userAgent string) {
	host = strings.TrimSpace(host)
	if host == "" {
		return
	}
	clientOverrideMu.Lock()
	defer clientOverrideMu.Unlock()
	if strings.TrimSpace(peerIDPrefix) == "" {
		delete(clientOverrides, host)
		return
	}
	clientOverrides[host] = ClientSpoof{
		PeerIDPrefix: strings.TrimSpace(peerIDPrefix),
		UserAgent:    strings.TrimSpace(userAgent),
	}
}

// GetClientOverrides returns a copy of the current spoof map.
func GetClientOverrides() map[string]ClientSpoof {
	clientOverrideMu.RLock()
	defer clientOverrideMu.RUnlock()
	out := make(map[string]ClientSpoof, len(clientOverrides))
	for k, v := range clientOverrides {
		out[k] = v
	}
	return out
}

// clientOverrideFor returns the spoof preset for a tracker URL, if any host
// substring matches.
func clientOverrideFor(trackerURL string) (ClientSpoof, bool) {
	clientOverrideMu.RLock()
	defer clientOverrideMu.RUnlock()
	for host, c := range clientOverrides {
		if strings.Contains(trackerURL, host) {
			return c, true
		}
	}
	return ClientSpoof{}, false
}

// Per-tracker secondary-announce stats mode (host-substring -> mode). Controls
// what byte counters the fire-and-forget secondary (dual-family) announce
// reports. The secondary exists to register our alternate-family address on
// trackers that only record the announce SOURCE ip (ignore BEP-7 &ipv4/&ipv6).
// It uses a XORed peer_id so the tracker holds a SECOND peer entry (dual-stack).
// But trackers that SUM stats across peer_ids then count our bytes twice.
//
//	"clone" (default) — secondary mirrors the primary uploaded/downloaded.
//	                    Safe on passkey-dedup/last-wins trackers (whichever
//	                    entry survives still carries the real stats).
//	"zero"            — secondary reports uploaded=0&downloaded=0 (pure address
//	                    advertisement). Correct on sum-by-peer_id trackers
//	                    (seedpool, torr9) where cloning doubles downloaded.
//	"off"             — secondary not sent at all.
//
// Seeded from config [announce_secondary_stats], hot via
// POST /api/announce/secondary-stats.
var (
	secondaryStatsMu        sync.RWMutex
	secondaryStatsOverrides = map[string]string{}
)

// InitSecondaryStatsOverrides seeds the mode map from config at boot.
func InitSecondaryStatsOverrides(fromConfig map[string]string) {
	secondaryStatsMu.Lock()
	defer secondaryStatsMu.Unlock()
	for host, mode := range fromConfig {
		h := strings.TrimSpace(host)
		m := strings.ToLower(strings.TrimSpace(mode))
		if h != "" && (m == "zero" || m == "off" || m == "clone") {
			secondaryStatsOverrides[h] = m
		}
	}
	if len(secondaryStatsOverrides) > 0 {
		slog.Info("tracker_announce: secondary-stats overrides active", "trackers", len(secondaryStatsOverrides))
	}
}

// ResetSecondaryStatsOverrides REPLACES the mode map (see ResetPasskeyOverrides).
func ResetSecondaryStatsOverrides(fromFront map[string]string) {
	secondaryStatsMu.Lock()
	clear(secondaryStatsOverrides)
	secondaryStatsMu.Unlock()
	InitSecondaryStatsOverrides(fromFront)
}

// SetSecondaryStatsOverride sets the secondary stats mode for trackers whose
// URL contains host. mode "" or "clone" clears (back to default). No restart.
func SetSecondaryStatsOverride(host, mode string) {
	host = strings.TrimSpace(host)
	if host == "" {
		return
	}
	m := strings.ToLower(strings.TrimSpace(mode))
	secondaryStatsMu.Lock()
	defer secondaryStatsMu.Unlock()
	if m == "" || m == "clone" {
		delete(secondaryStatsOverrides, host)
		return
	}
	if m == "zero" || m == "off" {
		secondaryStatsOverrides[host] = m
	}
}

// GetSecondaryStatsOverrides returns a copy of the current mode map.
func GetSecondaryStatsOverrides() map[string]string {
	secondaryStatsMu.RLock()
	defer secondaryStatsMu.RUnlock()
	out := make(map[string]string, len(secondaryStatsOverrides))
	for k, v := range secondaryStatsOverrides {
		out[k] = v
	}
	return out
}

// secondaryStatsModeFor returns the secondary stats mode for a tracker URL
// ("clone" if no override host substring matches).
func secondaryStatsModeFor(trackerURL string) string {
	secondaryStatsMu.RLock()
	defer secondaryStatsMu.RUnlock()
	for host, m := range secondaryStatsOverrides {
		if strings.Contains(trackerURL, host) {
			return m
		}
	}
	return "clone"
}

// Per-tracker announce address family (host-substring -> mode). A dual-stack
// host dialling plain "tcp" follows RFC 6724 and prefers IPv6, so every
// announce leaves over v6 and a tracker that records only the announce SOURCE
// ip ends up holding a v6 address for us. IPv4-only peers then get a peer entry
// they cannot dial, and we seed to nobody without a single error anywhere.
//
//	"auto" (default) — announce once per address family this host has, with the
//	                   same peer_id, so the tracker holds one peer reachable at
//	                   both addresses. This is what libtorrent does. On a
//	                   single-stack host it is a single announce, as before.
//	"v4"             — pin announces to this tracker to IPv4 only.
//	"v6"             — pin announces to this tracker to IPv6 only.
//
// Pin a family only for a tracker that mishandles the pair (counts the two
// addresses as two peers, or caps peers per account); "auto" is otherwise the
// reachable answer.
//
// Seeded from config [announce_ip_modes], hot via POST /api/announce/ip-modes.
// Ignored when a SOCKS announce proxy is configured: the egress family is then
// the proxy's, and forcing one here would break a proxy that is reachable over
// the other family only.
//
// There is deliberately no mode that stops announcing to a tracker. A client
// that can go silent on one tracker while staying in the swarm is what private
// trackers screen their whitelists for, and they screen on the capability, not
// on how carefully it is used -- from their side our silence is indistinguishable
// from a ratio cheat, because all they observe is an absence. Pausing a torrent
// already covers the honest case and does it better: it emits event=stopped, so
// we leave the swarm openly instead of vanishing from it.
const (
	AnnounceIPModeAuto = "auto"
	AnnounceIPModeV4   = "v4"
	AnnounceIPModeV6   = "v6"
)

var (
	announceIPModeMu sync.RWMutex
	announceIPModes  = map[string]string{}
)

// normalizeAnnounceIPMode maps user input onto a known mode, "" if unknown.
// Accepts the obvious aliases so the API is not a spelling test.
func normalizeAnnounceIPMode(mode string) string {
	switch strings.ToLower(strings.TrimSpace(mode)) {
	case "v4", "ipv4", "4", "inet", "inet4":
		return AnnounceIPModeV4
	case "v6", "ipv6", "6", "inet6":
		return AnnounceIPModeV6
	case "", "auto", "any", "default":
		return AnnounceIPModeAuto
	}
	return ""
}

// isRetiredSilentMode reports the spellings of the withdrawn "do not announce"
// mode. Kept only to recognise a config written by an older build: dropping such
// an entry silently would put a tracker an operator had switched off back into
// the announce rotation without a word, which on a private tracker is exactly
// the surprise they cannot afford.
func isRetiredSilentMode(mode string) bool {
	switch strings.ToLower(strings.TrimSpace(mode)) {
	case "none", "off", "block", "blocked", "disabled":
		return true
	}
	return false
}

// InitAnnounceIPModes seeds the family map from config at boot.
func InitAnnounceIPModes(fromConfig map[string]string) {
	announceIPModeMu.Lock()
	defer announceIPModeMu.Unlock()
	for host, mode := range fromConfig {
		h := strings.TrimSpace(host)
		if h != "" && isRetiredSilentMode(mode) {
			slog.Warn("tracker_announce: the announce mode that stopped announcing to a tracker has been withdrawn; this tracker is announced to again -- pause its torrents instead if that is not wanted",
				"tracker", h, "mode", mode)
			continue
		}
		m := normalizeAnnounceIPMode(mode)
		if h != "" && (m == AnnounceIPModeV4 || m == AnnounceIPModeV6) {
			announceIPModes[h] = m
		}
	}
	if len(announceIPModes) > 0 {
		slog.Info("tracker_announce: announce ip-family overrides active", "trackers", len(announceIPModes))
	}
}

// ResetAnnounceIPModes REPLACES the family map (see ResetPasskeyOverrides).
func ResetAnnounceIPModes(fromFront map[string]string) {
	announceIPModeMu.Lock()
	clear(announceIPModes)
	announceIPModeMu.Unlock()
	InitAnnounceIPModes(fromFront)
}

// ConfiguredAnnounceHosts returns every tracker host that carries a setting of
// ours: a client spoof or a passkey override.
//
// The live registry is deliberately O(hot), built from announces that actually
// happened, and it empties while torrents are paused. On its own that leaves an
// operator unable to reach the settings of a tracker they had already
// configured, precisely when nothing is running. These hosts cost nothing to
// list, since they are already in memory from the config.
func ConfiguredAnnounceHosts() []string {
	seen := map[string]bool{}
	clientOverrideMu.RLock()
	for host := range clientOverrides {
		seen[host] = true
	}
	clientOverrideMu.RUnlock()
	passkeyOverrideMu.RLock()
	for host := range passkeyOverrides {
		seen[host] = true
	}
	passkeyOverrideMu.RUnlock()
	out := make([]string, 0, len(seen))
	for host := range seen {
		out = append(out, host)
	}
	sort.Strings(out)
	return out
}

// SetAnnounceIPMode pins (or with "auto"/"" releases) the announce address
// family for trackers whose URL contains host. Applies to the next announce.
// Reports whether the mode was understood, so the API can reject a typo rather
// than silently leaving the tracker on auto.
func SetAnnounceIPMode(host, mode string) bool {
	host = strings.TrimSpace(host)
	if host == "" {
		return false
	}
	m := normalizeAnnounceIPMode(mode)
	if m == "" {
		return false
	}
	announceIPModeMu.Lock()
	defer announceIPModeMu.Unlock()
	if m == AnnounceIPModeAuto {
		delete(announceIPModes, host)
		return true
	}
	announceIPModes[host] = m
	return true
}

// GetAnnounceIPModes returns a copy of the current family map.
func GetAnnounceIPModes() map[string]string {
	announceIPModeMu.RLock()
	defer announceIPModeMu.RUnlock()
	out := make(map[string]string, len(announceIPModes))
	for k, v := range announceIPModes {
		out[k] = v
	}
	return out
}

// A tracker with no address in the companion family (no AAAA record, a
// filtered path, a v6 route that blackholes) would otherwise cost one dead
// connection per torrent per announce cycle. At six figures of torrents that
// is a flood of dials and of log lines for a fact that does not change by the
// minute, so remember it per (host, family) and stop trying for a while.
var (
	familyDeadMu sync.Mutex
	familyDead   = map[string]time.Time{}
)

const familyDeadFor = 30 * time.Minute

func familyIsDead(key string) bool {
	familyDeadMu.Lock()
	defer familyDeadMu.Unlock()
	until, ok := familyDead[key]
	if !ok {
		return false
	}
	if time.Now().After(until) {
		delete(familyDead, key)
		return false
	}
	return true
}

func markFamilyDead(key string) {
	familyDeadMu.Lock()
	defer familyDeadMu.Unlock()
	if _, seen := familyDead[key]; !seen {
		slog.Info("tracker_announce: companion announce family unreachable, pausing it", "target", key, "for", familyDeadFor)
	}
	familyDead[key] = time.Now().Add(familyDeadFor)
}

func markFamilyAlive(key string) {
	familyDeadMu.Lock()
	defer familyDeadMu.Unlock()
	delete(familyDead, key)
}

// AnnounceIPModeForHost reports the announce family pinned for a tracker host,
// "auto" when none is. Exported for the tracker listing the UI reads.
func AnnounceIPModeForHost(host string) string {
	return announceIPModeFor(host)
}

// announceIPModeFor returns the pinned family for a tracker URL or a dial
// target ("auto" if no override host substring matches).
func announceIPModeFor(trackerURL string) string {
	announceIPModeMu.RLock()
	defer announceIPModeMu.RUnlock()
	for host, m := range announceIPModes {
		if strings.Contains(trackerURL, host) {
			return m
		}
	}
	return AnnounceIPModeAuto
}

// applyPasskeyOverride rewrites the /announce/<passkey> path segment for any
// tracker whose URL contains a configured host; other trackers are untouched
// (safe on a multi-tracker instance).
func applyPasskeyOverride(trackerURL string) string {
	passkeyOverrideMu.RLock()
	defer passkeyOverrideMu.RUnlock()
	if len(passkeyOverrides) == 0 {
		return trackerURL
	}
	for host, pk := range passkeyOverrides {
		if !strings.Contains(trackerURL, host) {
			continue
		}
		const marker = "/announce/"
		i := strings.Index(trackerURL, marker)
		if i < 0 {
			continue
		}
		start := i + len(marker)
		end := len(trackerURL)
		for j := start; j < len(trackerURL); j++ {
			if trackerURL[j] == '/' || trackerURL[j] == '?' {
				end = j
				break
			}
		}
		return trackerURL[:start] + pk + trackerURL[end:]
	}
	return trackerURL
}

func loadAnnounceProxy() *url.URL {
	announceProxyOnce.Do(func() {
		raw := strings.TrimSpace(os.Getenv("TYPHON_ANNOUNCE_PROXY"))
		if raw == "" {
			return
		}
		u, err := url.Parse(raw)
		if err != nil || (u.Scheme != "socks5" && u.Scheme != "socks5h") {
			slog.Warn("tracker_announce: TYPHON_ANNOUNCE_PROXY ignored (parse failure or unsupported scheme)",
				"raw", raw, "error", err)
			return
		}
		announceProxyURL = u
		slog.Info("tracker_announce: SOCKS5 outbound proxy configured",
			"host", u.Host, "auth", u.User.Username() != "")
	})
	return announceProxyURL
}

// loadSecondaryAnnounceProxy reads TYPHON_ANNOUNCE_V6_PROXY (legacy name —
// in practice this is used as the dual-family secondary path: typically the
// primary egresses v6 and this secondary egresses v4). On private CloudFlare
// trackers, only the connection's source IP is registered (the &ip= param is
// ignored), so a single announce only registers us under one family. Firing a
// parallel announce through a second SOCKS5 proxy that exits via a different
// IP family lets the tracker register us as both v4 and v6 peer.
var (
	secondaryAnnounceProxyOnce sync.Once
	secondaryAnnounceProxyURL  *url.URL
)

func loadSecondaryAnnounceProxy() *url.URL {
	secondaryAnnounceProxyOnce.Do(func() {
		// [secondary announce removed 2026-08-01] force-disabled regardless of
		// env: the XORed 2nd peer_id registered a SECOND peer per torrent (torr9
		// x2 token hack) => the "duplicate peer" look on trackers. qBit/libtorrent
		// does v4/v6 with the SAME peer_id (one peer, two addresses). Rollback =
		// restore the env read below.
		raw := ""
		_ = os.Getenv("TYPHON_ANNOUNCE_V6_PROXY")
		if raw == "" {
			slog.Info("tracker_announce: secondary announce disabled (removed 2026-08-01)")
			return
		}
		u, err := url.Parse(raw)
		if err != nil || (u.Scheme != "socks5" && u.Scheme != "socks5h") {
			slog.Warn("tracker_announce: TYPHON_ANNOUNCE_V6_PROXY ignored (parse failure or unsupported scheme)",
				"raw", raw, "error", err)
			return
		}
		secondaryAnnounceProxyURL = u
		slog.Info("tracker_announce: secondary SOCKS5 outbound proxy configured",
			"host", u.Host, "auth", u.User.Username() != "")
	})
	return secondaryAnnounceProxyURL
}

// newTrackerAnnouncerForSession builds an announcer for a session that has no
// binding table of its own (the race engine), from that session's config.
//
// It exists because the bare-port shim it replaces built a Binding from the
// port alone. Every other announce-egress setting therefore defaulted to zero,
// announce_proxy included — so a relay setup whose hoard announced through the
// proxy had its race announces leave DIRECT, handing the tracker this host's
// own address. Worse than a plain bypass: with no proxy on the binding, the
// dual-family path also came alive and posted the same peer_id from the host's
// real address, which private trackers count as a second location.
//
// Going through DefaultSingleBinding + ApplyAnnounceEgress rather than filling
// a Binding in place is deliberate: it is the same pair main.go uses for the
// hoard, so a field added to the announce egress reaches both without anyone
// having to remember this call site.
func newTrackerAnnouncerForSession(port int, cfg *config.SessionConfig, scope string) *trackerAnnouncer {
	bs := ApplyAnnounceEgress(
		DefaultSingleBinding(port, cfg.EnableIPv6, scope, cfg.AnnounceRateLimit),
		cfg.AnnounceProxy, cfg.AnnounceIP, cfg.Socks5OutboundHost, scope)
	return newTrackerAnnouncerForBinding(bs[0])
}

// generateQBitPeerID generates a peer_id matching qBittorrent 4.6.1 format.
func generateQBitPeerID() string {
	prefix := version.PeerFingerprint()
	chars := "0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz"
	suffix := make([]byte, 12)
	for i := range suffix {
		suffix[i] = chars[rand.Intn(len(chars))]
	}
	return prefix + string(suffix)
}

// announce performs an HTTP tracker announce and returns the peer list.
// event: "started" (first announce), "" (periodic), "completed", "stopped".
// uploaded/downloaded are the per-torrent cumulative session totals (from the
// engine status: total_upload/total_download). Reporting them lets ratio-based
// private trackers credit our upload — historically these were hardcoded to 0,
// which froze tracker-side stats despite real transfer.
func (ta *trackerAnnouncer) announce(trackerURL, infoHash string, uploaded, downloaded, left int64, event string) (*TrackerAnnounceResult, error) {
	// Startup pause first: while held, nothing leaves at all, and there is no
	// point queueing on the rate limiter for something we will not send.
	if ta.gate.blocked() {
		return nil, ErrStartupPaused
	}
	// Gate every announce, http:// and udp:// alike, on the engine's
	// announce_rate_limit before a single byte leaves. No-op when unset.
	if err := ta.limiter.wait(context.Background()); err != nil {
		return nil, err
	}
	// Optional passkey override (env TYPHON_ANNOUNCE_PASSKEY) — rewrite the
	// /announce/<passkey> segment so this instance can announce under another
	// account without re-fetching every .torrent. Dormant unless the env is set.
	trackerURL = applyPasskeyOverride(trackerURL)
	// Optional per-tracker client spoof (peer_id prefix + User-Agent) to pass a
	// tracker's client whitelist. peerID keeps our stable suffix so the tracker
	// sees a consistent peer across announces; only the 8-byte prefix changes.
	peerID := ta.peerID
	userAgent := ta.userAgent
	spoof, spoofed := clientOverrideFor(trackerURL)
	if spoofed {
		if len(peerID) >= 20 && len(spoof.PeerIDPrefix) == 8 {
			peerID = spoof.PeerIDPrefix + peerID[8:]
		}
		if spoof.UserAgent != "" {
			userAgent = spoof.UserAgent
		}
	}
	// Build the announce URL with query parameters.
	u, err := url.Parse(trackerURL)
	if err != nil {
		return nil, fmt.Errorf("tracker announce: bad URL: %w", err)
	}

	// udp:// speaks a binary protocol on datagrams, not HTTP. It returns the
	// same result shape so nothing downstream has to care which one answered.
	// The passkey and client-spoof rewrites above are HTTP-shaped (URL path and
	// User-Agent) and simply do not apply on the wire here.
	if u.Scheme == "udp" {
		return ta.udpAnnounce(u, infoHash, uploaded, downloaded, left, event)
	}

	// info_hash must be URL-encoded raw bytes (20 bytes from hex).
	rawHash, err := hexToURLEncoded(infoHash)
	if err != nil {
		return nil, fmt.Errorf("tracker announce: bad info_hash: %w", err)
	}

	// Build URL manually to avoid double-encoding of info_hash.
	announceURL := u.Scheme + "://" + u.Host + u.Path + "?"
	if u.RawQuery != "" {
		announceURL += u.RawQuery + "&"
	}
	announceURL += "info_hash=" + rawHash
	announceURL += "&peer_id=" + url.QueryEscape(peerID)
	announcePort := ta.port
	if ta.livePort != nil {
		if v := ta.livePort.Load(); v > 0 {
			announcePort = int(v)
		}
	}
	announceURL += "&port=" + strconv.Itoa(announcePort)
	announceURL += "&uploaded=" + strconv.FormatInt(uploaded, 10)
	announceURL += "&downloaded=" + strconv.FormatInt(downloaded, 10)
	announceURL += "&left=" + strconv.FormatInt(left, 10)
	announceURL += "&compact=1&numwant=200"
	if event != "" {
		announceURL += "&event=" + event
	}
	// BEP-7 `&ip=` advertises the tracker-visible IP we'd like peers to
	// reach us at. Trackers that respect it will hand this address out to
	// other peers instead of (or in addition to) the source IP they observe.
	// Single-binding leaves PublicIP empty — the source IP is correct.
	if ta.publicIP != "" {
		announceURL += "&ip=" + url.QueryEscape(ta.publicIP)
	}

	// Fire-and-forget secondary announce via the V6_PROXY SOCKS5 (egress
	// path complementary to the primary — typically v4 when primary egresses
	// v6). peer_id last byte is XORed so the tracker stores both entries
	// instead of overwriting on dedup-by-peer_id (some trackers behave this way).
	// Skip the secondary (dual-family) announce for spoofed trackers: it is a
	// torr9-specific double-credit trick and would post a second qB peer to MAM.
	secStatsMode := secondaryStatsModeFor(trackerURL)
	if ta.secondaryClient != nil && !spoofed && secStatsMode != "off" && len(ta.peerID) >= 20 {
		pidSec := []byte(ta.peerID)
		pidSec[19] ^= 0x01
		secURL := strings.Replace(
			announceURL,
			"peer_id="+url.QueryEscape(ta.peerID),
			"peer_id="+url.QueryEscape(string(pidSec)),
			1,
		)
		if secStatsMode == "zero" {
			// The secondary exists only to advertise the alternate-family
			// address for reachability — it must carry NO stats, otherwise a
			// sum-by-peer_id tracker double-counts our bytes (downloaded 2x =
			// ratio halved on torr9, which MAXes upload but sums download).
			secURL = strings.Replace(secURL, "&uploaded="+strconv.FormatInt(uploaded, 10), "&uploaded=0", 1)
			secURL = strings.Replace(secURL, "&downloaded="+strconv.FormatInt(downloaded, 10), "&downloaded=0", 1)
		}
		secondaryClient := ta.secondaryClient
		userAgent := ta.userAgent
		limiter := ta.limiter
		go func() {
			// The secondary is a second request on the wire, so it draws its
			// own token. Fire-and-forget already, so waiting costs nothing.
			if err := limiter.wait(context.Background()); err != nil {
				slog.Warn("tracker_announce secondary: rate limited", "error", err)
				return
			}
			req, err := http.NewRequest("GET", secURL, nil)
			if err != nil {
				slog.Warn("tracker_announce secondary: build req failed", "error", err)
				return
			}
			req.Header.Set("User-Agent", userAgent)
			resp, err := secondaryClient.Do(req)
			if err != nil {
				slog.Warn("tracker_announce secondary: request failed", "error", err)
				return
			}
			defer resp.Body.Close()
			io.Copy(io.Discard, io.LimitReader(resp.Body, 1<<20))
			if resp.StatusCode != 200 {
				slog.Warn("tracker_announce secondary: HTTP non-2xx", "status", resp.StatusCode)
			}
		}()
	}

	// Dual-family announce. A tracker that records only the announce source
	// address holds whichever family we left by, so announcing once makes us
	// unreachable for peers on the other one — silently, since nothing fails.
	// libtorrent answers this by announcing from every listen socket with the
	// SAME peer_id: one peer, two addresses. Match that, because being compared
	// against qBittorrent is the point.
	//
	// "auto" therefore means both families, not "let the kernel pick one". The
	// v4/v6 modes stay available and pin the family in the dialer instead.
	primary := ta.httpClient
	var companion *http.Client
	var companionKey string
	if announceIPModeFor(trackerURL) == AnnounceIPModeAuto && ta.clientV4 != nil && ta.clientV6 != nil {
		// Pick the primary deterministically rather than inheriting RFC 6724's
		// choice: the caller parses the primary's peer list, and we want to know
		// which family produced it.
		primary = ta.clientV4
		companion = ta.clientV6
		if u, err := url.Parse(trackerURL); err == nil {
			companionKey = u.Host + "|v6"
		}
	}
	if companion != nil && secondaryStatsModeFor(trackerURL) != "off" && !familyIsDead(companionKey) {
		companionClient, companionURL, key := companion, announceURL, companionKey
		ua, limiter := userAgent, ta.limiter
		go func() {
			// A second request on the wire draws its own token. Fire-and-forget
			// already, so waiting costs nothing.
			if err := limiter.wait(context.Background()); err != nil {
				return
			}
			req, err := http.NewRequest("GET", companionURL, nil)
			if err != nil {
				return
			}
			req.Header.Set("User-Agent", ua)
			resp, err := companionClient.Do(req)
			if err != nil {
				markFamilyDead(key)
				return
			}
			defer resp.Body.Close()
			io.Copy(io.Discard, io.LimitReader(resp.Body, 1<<20))
			if resp.StatusCode != 200 {
				markFamilyDead(key)
				return
			}
			markFamilyAlive(key)
		}()
	}

	req, err := http.NewRequest("GET", announceURL, nil)
	if err != nil {
		return nil, fmt.Errorf("tracker announce: %w", err)
	}
	req.Header.Set("User-Agent", userAgent)

	resp, err := primary.Do(req)
	if err != nil {
		return nil, fmt.Errorf("tracker announce: %w", err)
	}
	defer resp.Body.Close()

	if resp.StatusCode != 200 {
		return nil, fmt.Errorf("tracker announce: HTTP %d", resp.StatusCode)
	}

	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20)) // 1MB max
	if err != nil {
		return nil, fmt.Errorf("tracker announce: read body: %w", err)
	}

	return parseTrackerResponse(body, ta.enableIPv6)
}

// hexToURLEncoded converts a hex info_hash to percent-encoded binary form.
func hexToURLEncoded(hexHash string) (string, error) {
	if len(hexHash) != 40 {
		return "", fmt.Errorf("invalid info_hash length: %d", len(hexHash))
	}
	var sb strings.Builder
	for i := 0; i < 40; i += 2 {
		b, err := strconv.ParseUint(hexHash[i:i+2], 16, 8)
		if err != nil {
			return "", err
		}
		fmt.Fprintf(&sb, "%%%02x", b)
	}
	return sb.String(), nil
}

// parseTrackerResponse parses a bencoded tracker response. `acceptV6` gates
// the BEP-7 `peers6` field: off, the response is read exactly as before.
func parseTrackerResponse(data []byte, acceptV6 bool) (*TrackerAnnounceResult, error) {
	dict, err := bdecodeDict(data)
	if err != nil {
		return nil, fmt.Errorf("tracker response: %w", err)
	}

	result := &TrackerAnnounceResult{}

	if reason, ok := dict["failure reason"]; ok {
		if s, ok := reason.(string); ok {
			result.FailureReason = s
			return result, nil
		}
	}

	if v, ok := dict["interval"]; ok {
		if n, ok := v.(int64); ok {
			result.Interval = int(n)
		}
	}
	if v, ok := dict["complete"]; ok {
		if n, ok := v.(int64); ok {
			result.Complete = int(n)
		}
	}
	if v, ok := dict["incomplete"]; ok {
		if n, ok := v.(int64); ok {
			result.Incomplete = int(n)
		}
	}

	// Parse compact peer list (6 bytes per peer: 4 IP + 2 port).
	if peersRaw, ok := dict["peers"]; ok {
		switch v := peersRaw.(type) {
		case string:
			data := []byte(v)
			for i := 0; i+6 <= len(data); i += 6 {
				ip := fmt.Sprintf("%d.%d.%d.%d", data[i], data[i+1], data[i+2], data[i+3])
				port := int(data[i+4])<<8 + int(data[i+5])
				if port > 0 {
					result.Peers = append(result.Peers, TrackerPeer{IP: ip, Port: port})
				}
			}
		case []byte:
			for i := 0; i+6 <= len(v); i += 6 {
				ip := fmt.Sprintf("%d.%d.%d.%d", v[i], v[i+1], v[i+2], v[i+3])
				port := int(v[i+4])<<8 + int(v[i+5])
				if port > 0 {
					result.Peers = append(result.Peers, TrackerPeer{IP: ip, Port: port})
				}
			}
		}
	}

	// Compact IPv6 peer list (BEP 7, 18 bytes per peer: 16 IP + 2 port).
	// Trackers send it alongside `peers`, never instead of it, so this only
	// ever adds to what we already had.
	if acceptV6 {
		if raw, ok := dict["peers6"]; ok {
			var data []byte
			switch v := raw.(type) {
			case string:
				data = []byte(v)
			case []byte:
				data = v
			}
			for i := 0; i+18 <= len(data); i += 18 {
				port := int(data[i+16])<<8 + int(data[i+17])
				if port == 0 {
					continue
				}
				ip := net.IP(data[i : i+16])
				// A v4-mapped entry is a v4 peer in disguise; unwrap it so it
				// reads like every other v4 address downstream.
				if v4 := ip.To4(); v4 != nil {
					result.Peers = append(result.Peers, TrackerPeer{IP: v4.String(), Port: port})
					continue
				}
				result.Peers = append(result.Peers, TrackerPeer{IP: ip.String(), Port: port})
			}
		}
	}

	return result, nil
}

// ---------------------------------------------------------------------------
// Minimal bencode decoder (just enough for tracker responses)
// ---------------------------------------------------------------------------

func bdecodeDict(data []byte) (map[string]interface{}, error) {
	if len(data) == 0 || data[0] != 'd' {
		return nil, fmt.Errorf("not a bencoded dict")
	}
	val, _, err := bdecodeValue(data)
	if err != nil {
		return nil, err
	}
	if dict, ok := val.(map[string]interface{}); ok {
		return dict, nil
	}
	return nil, fmt.Errorf("top-level value is not a dict")
}

func bdecodeValue(data []byte) (interface{}, int, error) {
	if len(data) == 0 {
		return nil, 0, fmt.Errorf("empty data")
	}
	switch data[0] {
	case 'd':
		return bdecodeMap(data)
	case 'l':
		return bdecodeList(data)
	case 'i':
		return bdecodeInt(data)
	default:
		if data[0] >= '0' && data[0] <= '9' {
			return bdecodeString(data)
		}
		return nil, 0, fmt.Errorf("unexpected byte: %c", data[0])
	}
}

func bdecodeMap(data []byte) (map[string]interface{}, int, error) {
	if data[0] != 'd' {
		return nil, 0, fmt.Errorf("not a dict")
	}
	pos := 1
	dict := make(map[string]interface{})
	for pos < len(data) && data[pos] != 'e' {
		keyVal, n, err := bdecodeString(data[pos:])
		if err != nil {
			return nil, 0, err
		}
		pos += n
		key := keyVal.(string)

		val, n, err := bdecodeValue(data[pos:])
		if err != nil {
			return nil, 0, err
		}
		pos += n
		dict[key] = val
	}
	if pos >= len(data) {
		return nil, 0, fmt.Errorf("unterminated dict")
	}
	return dict, pos + 1, nil // +1 for 'e'
}

func bdecodeList(data []byte) ([]interface{}, int, error) {
	if data[0] != 'l' {
		return nil, 0, fmt.Errorf("not a list")
	}
	pos := 1
	var list []interface{}
	for pos < len(data) && data[pos] != 'e' {
		val, n, err := bdecodeValue(data[pos:])
		if err != nil {
			return nil, 0, err
		}
		pos += n
		list = append(list, val)
	}
	if pos >= len(data) {
		return nil, 0, fmt.Errorf("unterminated list")
	}
	return list, pos + 1, nil
}

func bdecodeInt(data []byte) (int64, int, error) {
	if data[0] != 'i' {
		return 0, 0, fmt.Errorf("not an int")
	}
	end := bytes.IndexByte(data, 'e')
	if end < 0 {
		return 0, 0, fmt.Errorf("unterminated int")
	}
	n, err := strconv.ParseInt(string(data[1:end]), 10, 64)
	if err != nil {
		return 0, 0, err
	}
	return n, end + 1, nil
}

func bdecodeString(data []byte) (interface{}, int, error) {
	colonIdx := bytes.IndexByte(data, ':')
	if colonIdx < 0 {
		return nil, 0, fmt.Errorf("no colon in string")
	}
	length, err := strconv.Atoi(string(data[:colonIdx]))
	if err != nil {
		return nil, 0, err
	}
	start := colonIdx + 1
	if start+length > len(data) {
		return nil, 0, fmt.Errorf("string overflows data")
	}
	raw := data[start : start+length]
	// Return as string — the compact peers field needs raw bytes,
	// but Go strings can hold arbitrary bytes.
	return string(raw), start + length, nil
}

// ---------------------------------------------------------------------------
// Tracker URL extraction from .torrent files
// ---------------------------------------------------------------------------

// trackerURLFromTorrentFile extracts the announce URL from a .torrent file.
func trackerURLFromTorrentFile(data []byte) string {
	dict, err := bdecodeDict(data)
	if err != nil {
		return ""
	}
	if announce, ok := dict["announce"]; ok {
		if s, ok := announce.(string); ok {
			return s
		}
	}
	return ""
}

// ---------------------------------------------------------------------------
// Aggressive announce loop for race torrents
// ---------------------------------------------------------------------------

// raceAnnounceLoop performs aggressive tracker announces for a newly added race torrent.
// Phase 1: every 5s for 60s (12 attempts)
// Phase 2: every 30s until the torrent completes or is removed (safety cap 6h)
// Stops when the torrent is complete or removed.
func (e *RaceEngine) raceAnnounceLoop(infoHash, trackerURL string, totalSize int64) {
	if trackerURL == "" {
		slog.Warn("race: no tracker URL, skipping announce loop", "info_hash", infoHash[:minStr(len(infoHash), 8)])
		return
	}

	announcer := e.announcer()

	slog.Info("race: starting announce loop",
		"info_hash", infoHash[:minStr(len(infoHash), 8)],
		"tracker", trackerURL)

	// Phase 1: aggressive — every 5s for 60s.
	if e.announcePhase(infoHash, trackerURL, totalSize, announcer, 5*time.Second, 60*time.Second) {
		return
	}

	// Phase 2: sustained — every 30s until torrent completes (safety cap 6h).
	// Private-tracker races rarely exceed 5 peers, so the old "stop at >0 peers"
	// rule killed the loop immediately. We now feed the swarm with fresh peers
	// throughout the download.
	e.announcePhase(infoHash, trackerURL, totalSize, announcer, 30*time.Second, 6*time.Hour)
}

// announcePhase runs announces at a fixed interval for a duration.
// Returns true if the loop should stop (peers found, torrent removed, or context cancelled).
func (e *RaceEngine) announcePhase(infoHash, trackerURL string, totalSize int64, announcer *trackerAnnouncer, interval, duration time.Duration) bool {
	ticker := time.NewTicker(interval)
	defer ticker.Stop()
	deadline := time.After(duration)

	// Do one announce immediately at start of phase.
	if e.doAnnounceAndInject(infoHash, trackerURL, totalSize, announcer) {
		return true
	}

	for {
		select {
		case <-e.ctx.Done():
			return true
		case <-deadline:
			return false
		case <-ticker.C:
			if e.doAnnounceAndInject(infoHash, trackerURL, totalSize, announcer) {
				return true
			}
		}
	}
}

// doAnnounceAndInject performs a single announce, injects peers, and fires the announce event.
// Returns true if the loop should stop (torrent removed or peers already found).
func (e *RaceEngine) doAnnounceAndInject(infoHash, trackerURL string, totalSize int64, announcer *trackerAnnouncer) bool {
	// Check torrent still exists.
	e.mu.RLock()
	_, exists := e.torrents[infoHash]
	e.mu.RUnlock()
	if !exists {
		return true
	}

	// Stop only when the torrent finishes downloading. Private-tracker races
	// stabilize at 2-5 peers; bailing on NumPeers>0 (the old behavior) killed
	// the announce loop after the first response — often just an injected
	// uploader-intel candidate — and the swarm never grew.
	s, err := e.client.GetStatus(infoHash)
	if err != nil {
		return true // torrent gone
	}
	if s.TotalSize > 0 && s.TotalDone >= s.TotalSize {
		slog.Info("race: torrent complete, stopping announce loop",
			"info_hash", infoHash[:minStr(len(infoHash), 8)])
		return true
	}

	// Determine left bytes.
	left := totalSize - s.TotalDone
	if left < 0 {
		left = 0
	}

	result, err := announcer.announce(trackerURL, infoHash, s.TotalUpload, s.TotalDownload, left, "started")
	if err != nil {
		slog.Debug("race: announce failed",
			"info_hash", infoHash[:minStr(len(infoHash), 8)],
			"error", err)
		return false // retry
	}

	if result.FailureReason != "" {
		slog.Warn("race: tracker failure",
			"info_hash", infoHash[:minStr(len(infoHash), 8)],
			"reason", result.FailureReason)
		return false
	}

	// Fire announce event.
	if e.onEvent != nil {
		e.mu.RLock()
		info := e.torrents[infoHash]
		addedAt := e.addedTime[infoHash]
		e.mu.RUnlock()
		var name, cat string
		if info != nil {
			name = info.Name
			cat = info.Category
		}
		go e.onEvent("announce", TorrentStats{
			InfoHash:      infoHash,
			Name:          name,
			Category:      cat,
			AddedTime:     addedAt.Unix(),
			SwarmSeeds:    result.Complete,
			SwarmLeechers: result.Incomplete,
			NumPeers:      len(result.Peers),
		})
	}

	// Update swarm data on every successful announce, even when peer list is
	// empty — otherwise stale Seeds/Leechers from the first announce stick
	// forever once the swarm empties out.
	e.mu.Lock()
	if e.swarmData[infoHash] == nil {
		e.swarmData[infoHash] = &SwarmData{}
	}
	e.swarmData[infoHash].Seeds = result.Complete
	e.swarmData[infoHash].Leechers = result.Incomplete
	e.swarmData[infoHash].LastSeen = time.Now()
	e.mu.Unlock()

	// Inject peers into libtorrent via add_peers.
	if len(result.Peers) > 0 {
		peers := make([]struct {
			IP   string
			Port int
		}, len(result.Peers))
		for i, p := range result.Peers {
			peers[i] = struct {
				IP   string
				Port int
			}{p.IP, p.Port}
		}
		e.client.AddPeers(infoHash, peers)

		slog.Info("race: announce got peers, injected",
			"info_hash", infoHash[:minStr(len(infoHash), 8)],
			"peers", len(result.Peers),
			"seeds", result.Complete,
			"leechers", result.Incomplete)

		// Keep announcing — we want to keep feeding the swarm with fresh peers
		// throughout the download. Completion check above is the only stop.
		return false
	}

	slog.Debug("race: announce OK but 0 peers",
		"info_hash", infoHash[:minStr(len(infoHash), 8)],
		"seeds", result.Complete,
		"leechers", result.Incomplete)

	return false
}
