package engine

import (
	"bytes"
	"context"
	"crypto/tls"
	"fmt"
	"github.com/Kheopsian/hydra/internal/version"
	"io"
	"log/slog"
	"math/rand"
	"net"
	"net/http"
	"net/url"
	"os"
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
type trackerAnnouncer struct {
	httpClient      *http.Client
	secondaryClient *http.Client // optional, fire-and-forget via TYPHON_ANNOUNCE_V6_PROXY (v4 egress path)
	peerID          string
	port            int
	publicIP        string
	userAgent       string
	bindingID       int
	livePort        *atomic.Int64 // runtime port override (nil/0 = use static port)
	fwmark          int           // SO_MARK for this binding's egress; also used by the udp:// path
	enableIPv6      bool          // take the tracker's BEP-7 `peers6` list
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
	dial := dialer.DialContext
	if !b.EnableIPv6 {
		base := dialer
		dial = func(ctx context.Context, network, addr string) (net.Conn, error) {
			return base.DialContext(ctx, ipv4Network(network), addr)
		}
	}

	transport := &http.Transport{
		TLSClientConfig:   &tls.Config{InsecureSkipVerify: true},
		DisableKeepAlives: true,
		DialContext:       dial,
	}
	// If a SOCKS5 outbound proxy is configured (TYPHON_ANNOUNCE_PROXY env),
	// route every announce through it. The base dialer above is reused to
	// reach the proxy itself (so SO_MARK still applies if the proxy lives
	// behind a fwmark-routed tunnel — though typical setup is the proxy at
	// a globally-reachable v6 addr that the kernel routes via the default
	// table, in which case Fwmark=0 and no marking happens).
	if pu := loadAnnounceProxy(); pu != nil {
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
			TLSClientConfig:   &tls.Config{InsecureSkipVerify: true},
			DisableKeepAlives: true,
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
		peerID:          peerID,
		port:            port,
		publicIP:        b.PublicIP,
		userAgent:       version.UserAgent(),
		bindingID:       b.ID,
		fwmark:          int(b.Fwmark),
		enableIPv6:      b.EnableIPv6,
		limiter:         announceLimiterFor(b.AnnounceScope+"#"+strconv.Itoa(b.ID), b.AnnounceRateLimit),
		gate:            startupGateFor(b.AnnounceScope),
	}
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

// newTrackerAnnouncer is a thin shim retained for callers that still pass a
// bare port (race aggressive announce). Equivalent to a single-binding setup
// with no source-IP override.
func newTrackerAnnouncer(port int, announceRateLimit float64) *trackerAnnouncer {
	return newTrackerAnnouncerForBinding(Binding{
		ID:                0,
		ListenPort:        port,
		AnnounceScope:     "race",
		AnnounceRateLimit: announceRateLimit,
	})
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

	req, err := http.NewRequest("GET", announceURL, nil)
	if err != nil {
		return nil, fmt.Errorf("tracker announce: %w", err)
	}
	req.Header.Set("User-Agent", userAgent)

	resp, err := ta.httpClient.Do(req)
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

	announcer := newTrackerAnnouncer(e.config.ListenPort, e.config.AnnounceRateLimit)

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
