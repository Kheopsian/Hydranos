package engine

import (
	"context"
	"crypto/rand"
	"encoding/binary"
	"encoding/hex"
	"fmt"
	"log/slog"
	"net"
	"net/url"
	"strings"
	"sync"
	"time"
)

// UDP tracker protocol (BEP 15). Trackers announced over udp:// used to be
// skipped outright, which on a magnet whose only trackers are udp:// left the
// DHT as the sole way to find a swarm.
//
// The exchange is two round trips: a connect that returns a connection_id, then
// the announce itself carrying that id. The id expires after a minute, so it is
// cached per tracker endpoint rather than re-negotiated for every torrent: at
// 100k torrents sharing a handful of trackers, one connect per torrent would be
// a self-inflicted flood.
const (
	udpProtocolID = 0x41727101980 // fixed magic, first field of every connect

	udpActionConnect  = 0
	udpActionAnnounce = 1
	udpActionError    = 3

	// The tracker may hold a connection_id for a minute; expire ours earlier so
	// an announce is never sent with an id the tracker has just dropped.
	udpConnIDTTL = 45 * time.Second
)

// BEP 15 event codes. The HTTP side spells these as words; the wire wants ints.
var udpEventCode = map[string]uint32{
	"":          0,
	"completed": 1,
	"started":   2,
	"stopped":   3,
}

// isSupportedTrackerScheme reports whether we can announce to this URL at all.
// Both announcers (race and hoard) walk a torrent's tracker tiers and need the
// same answer; keeping it in one place is what stops udp:// from being enabled
// on one path and silently skipped on the other.
func isSupportedTrackerScheme(u string) bool {
	return strings.HasPrefix(u, "http://") ||
		strings.HasPrefix(u, "https://") ||
		strings.HasPrefix(u, "udp://")
}

type udpConnID struct {
	id       int64
	obtained time.Time
}

var (
	udpConnMu    sync.Mutex
	udpConnCache = map[string]udpConnID{}
)

// cachedConnID returns a still-valid connection_id for this endpoint, if any.
// Keyed by fwmark as well as address: two bindings reach the same tracker
// through different tunnels, and a connection_id is bound to the source the
// tracker saw.
func cachedConnID(key string) (int64, bool) {
	udpConnMu.Lock()
	defer udpConnMu.Unlock()
	e, ok := udpConnCache[key]
	if !ok || time.Since(e.obtained) > udpConnIDTTL {
		return 0, false
	}
	return e.id, true
}

func storeConnID(key string, id int64) {
	udpConnMu.Lock()
	udpConnCache[key] = udpConnID{id: id, obtained: time.Now()}
	udpConnMu.Unlock()
}

func dropConnID(key string) {
	udpConnMu.Lock()
	delete(udpConnCache, key)
	udpConnMu.Unlock()
}

// udpAnnounce performs a BEP 15 announce and returns the same result shape as
// the HTTP path, so callers cannot tell the two apart.
func (ta *trackerAnnouncer) udpAnnounce(u *url.URL, infoHash string, uploaded, downloaded, left int64, event string) (*TrackerAnnounceResult, error) {
	// A SOCKS5 proxy carries TCP; relaying UDP needs UDP ASSOCIATE, which we do
	// not implement. Falling back to a direct datagram would send our real
	// address to the tracker precisely when the operator asked for the opposite,
	// so refuse instead. Same rule as the magnet resolver: never silently
	// bypass the tunnel.
	if ta.proxied {
		return nil, fmt.Errorf("udp tracker skipped: an announce proxy is configured and UDP cannot be proxied")
	}

	ih, err := hex.DecodeString(infoHash)
	if err != nil || len(ih) != 20 {
		return nil, fmt.Errorf("udp announce: bad info_hash %q", infoHash)
	}
	pid := []byte(ta.peerID)
	if len(pid) != 20 {
		return nil, fmt.Errorf("udp announce: peer_id must be 20 bytes, got %d", len(pid))
	}
	code, ok := udpEventCode[event]
	if !ok {
		return nil, fmt.Errorf("udp announce: unknown event %q", event)
	}

	host := u.Host
	if u.Port() == "" {
		host = net.JoinHostPort(u.Hostname(), "80")
	}
	key := fmt.Sprintf("%s|%d", host, ta.fwmark)

	dialer := &net.Dialer{Timeout: 5 * time.Second}
	// Device pin, not source pin, and no LocalAddr: this dial is UDP, so a
	// *net.TCPAddr source would be the wrong type entirely. The device is what
	// steers the egress anyway.
	applyEgressControl(dialer, ta.bindInterface, ta.fwmark)
	if name := ta.bindInterface; name != "" {
		if _, ierr := net.InterfaceByName(name); ierr != nil {
			return nil, fmt.Errorf("udp announce: bind_interface %q does not resolve, refusing to announce from the default route: %w", name, ierr)
		}
	}
	ctx, cancel := context.WithTimeout(context.Background(), 20*time.Second)
	defer cancel()
	conn, err := dialer.DialContext(ctx, "udp", host)
	if err != nil {
		return nil, fmt.Errorf("udp announce: dial %s: %w", host, err)
	}
	defer conn.Close()

	connID, cached := cachedConnID(key)
	if !cached {
		connID, err = udpConnect(conn)
		if err != nil {
			return nil, err
		}
		storeConnID(key, connID)
	}

	res, err := ta.udpDoAnnounce(conn, connID, ih, pid, uploaded, downloaded, left, code)
	if err == nil {
		return res, nil
	}
	// A cached id the tracker has already forgotten is the one failure worth
	// retrying: re-connect once and replay. Anything else is reported as is.
	if cached {
		dropConnID(key)
		connID, cerr := udpConnect(conn)
		if cerr != nil {
			return nil, cerr
		}
		storeConnID(key, connID)
		return ta.udpDoAnnounce(conn, connID, ih, pid, uploaded, downloaded, left, code)
	}
	return nil, err
}

// udpConnect performs the handshake that yields a connection_id.
func udpConnect(conn net.Conn) (int64, error) {
	txn := randomTxnID()
	req := make([]byte, 16)
	binary.BigEndian.PutUint64(req[0:8], udpProtocolID)
	binary.BigEndian.PutUint32(req[8:12], udpActionConnect)
	binary.BigEndian.PutUint32(req[12:16], txn)

	resp, err := udpRoundTrip(conn, req, 16)
	if err != nil {
		return 0, fmt.Errorf("udp connect: %w", err)
	}
	action := binary.BigEndian.Uint32(resp[0:4])
	gotTxn := binary.BigEndian.Uint32(resp[4:8])
	if gotTxn != txn {
		return 0, fmt.Errorf("udp connect: transaction id mismatch")
	}
	if action == udpActionError {
		return 0, fmt.Errorf("udp connect: tracker error: %s", udpErrorText(resp))
	}
	if action != udpActionConnect {
		return 0, fmt.Errorf("udp connect: unexpected action %d", action)
	}
	return int64(binary.BigEndian.Uint64(resp[8:16])), nil
}

func (ta *trackerAnnouncer) udpDoAnnounce(conn net.Conn, connID int64, ih, pid []byte, uploaded, downloaded, left int64, event uint32) (*TrackerAnnounceResult, error) {
	announcePort := ta.port
	if ta.livePort != nil {
		if v := ta.livePort.Load(); v > 0 {
			announcePort = int(v)
		}
	}

	txn := randomTxnID()
	req := make([]byte, 98)
	binary.BigEndian.PutUint64(req[0:8], uint64(connID))
	binary.BigEndian.PutUint32(req[8:12], udpActionAnnounce)
	binary.BigEndian.PutUint32(req[12:16], txn)
	copy(req[16:36], ih)
	copy(req[36:56], pid)
	binary.BigEndian.PutUint64(req[56:64], uint64(downloaded))
	binary.BigEndian.PutUint64(req[64:72], uint64(left))
	binary.BigEndian.PutUint64(req[72:80], uint64(uploaded))
	binary.BigEndian.PutUint32(req[80:84], event)
	// IP 0 means "use the source address you see", which is what we want: the
	// packet already leaves through the right tunnel, so the tracker's view is
	// the correct one. Announcing an IP we picked ourselves is how you end up
	// publishing a home address through a VPN.
	binary.BigEndian.PutUint32(req[84:88], 0)
	binary.BigEndian.PutUint32(req[88:92], randomTxnID()) // key
	binary.BigEndian.PutUint32(req[92:96], 200)           // num_want
	binary.BigEndian.PutUint16(req[96:98], uint16(announcePort))

	resp, err := udpRoundTrip(conn, req, 20)
	if err != nil {
		return nil, fmt.Errorf("udp announce: %w", err)
	}
	action := binary.BigEndian.Uint32(resp[0:4])
	gotTxn := binary.BigEndian.Uint32(resp[4:8])
	if gotTxn != txn {
		return nil, fmt.Errorf("udp announce: transaction id mismatch")
	}
	if action == udpActionError {
		msg := udpErrorText(resp)
		return &TrackerAnnounceResult{FailureReason: msg}, fmt.Errorf("udp announce: tracker error: %s", msg)
	}
	if action != udpActionAnnounce {
		return nil, fmt.Errorf("udp announce: unexpected action %d", action)
	}

	res := &TrackerAnnounceResult{
		Interval:   int(binary.BigEndian.Uint32(resp[8:12])),
		Incomplete: int(binary.BigEndian.Uint32(resp[12:16])),
		Complete:   int(binary.BigEndian.Uint32(resp[16:20])),
	}
	// Which family the peer list uses follows the transport we reached the
	// tracker on, not the payload length.
	v6 := false
	if ra, ok := conn.RemoteAddr().(*net.UDPAddr); ok && ra.IP.To4() == nil {
		v6 = true
	}
	res.Peers = parseUDPPeers(resp[20:], v6)
	return res, nil
}

// parseUDPPeers reads the compact peer list trailing an announce response.
// BEP 15 only defines 6-byte IPv4 entries; a tracker reached over IPv6 answers
// with 18-byte ones instead.
//
// The layout cannot be inferred from the payload length: every 18-byte v6 list
// is also a whole number of 6-byte v4 entries, so guessing by divisibility
// decodes v6 peers as garbage v4 addresses. The transport family is the only
// reliable signal, hence v6 comes from the socket we just used. The length is
// still checked, and the other stride tried, for a tracker that answers in the
// family we did not reach it on.
func parseUDPPeers(b []byte, v6 bool) []TrackerPeer {
	if len(b) == 0 {
		return nil
	}
	stride := 6
	if v6 {
		stride = 18
	}
	if len(b)%stride != 0 {
		alt := 6
		if stride == 6 {
			alt = 18
		}
		if len(b)%alt != 0 {
			return nil
		}
		stride = alt
	}
	peers := make([]TrackerPeer, 0, len(b)/stride)
	for i := 0; i+stride <= len(b); i += stride {
		ipLen := stride - 2
		ip := net.IP(append([]byte(nil), b[i:i+ipLen]...))
		port := int(binary.BigEndian.Uint16(b[i+ipLen : i+stride]))
		if port == 0 || ip.IsUnspecified() {
			continue
		}
		peers = append(peers, TrackerPeer{IP: ip.String(), Port: port})
	}
	return peers
}

// udpRoundTrip sends one datagram and waits for a reply of at least minLen.
// BEP 15 prescribes retrying at 15 * 2^n seconds; that is tuned for a client
// with nothing else to do. Hydra announces on a schedule and has a DHT to fall
// back on, so it tries three times over ~14s rather than parking a goroutine
// for minutes per dead tracker.
func udpRoundTrip(conn net.Conn, req []byte, minLen int) ([]byte, error) {
	timeouts := []time.Duration{2 * time.Second, 4 * time.Second, 8 * time.Second}
	buf := make([]byte, 4096)
	var lastErr error
	for _, to := range timeouts {
		if err := conn.SetDeadline(time.Now().Add(to)); err != nil {
			return nil, err
		}
		if _, err := conn.Write(req); err != nil {
			lastErr = err
			continue
		}
		for {
			n, err := conn.Read(buf)
			if err != nil {
				lastErr = err
				break
			}
			// A late reply to a previous attempt can still be sitting in the
			// socket; anything too short to hold a header is not our answer.
			if n < minLen {
				continue
			}
			return append([]byte(nil), buf[:n]...), nil
		}
	}
	if lastErr == nil {
		lastErr = fmt.Errorf("no response")
	}
	return nil, lastErr
}

func udpErrorText(resp []byte) string {
	if len(resp) <= 8 {
		return "(no message)"
	}
	return string(resp[8:])
}

func randomTxnID() uint32 {
	var b [4]byte
	if _, err := rand.Read(b[:]); err != nil {
		// Only reachable if the OS entropy source fails; a fixed id still
		// round-trips, it just loses its value as a reply filter.
		slog.Warn("tracker_udp: entropy read failed, using a fixed transaction id", "err", err)
		return 0x48594452
	}
	return binary.BigEndian.Uint32(b[:])
}
