package engine

import (
	"encoding/binary"
	"encoding/hex"
	"net"
	"net/url"
	"sync"
	"testing"
	"time"
)

// fakeUDPTracker speaks just enough BEP 15 to answer one client, and records
// what it was told. Testing against a public tracker would announce a real
// address into a real swarm, which is not something a test should do.
type fakeUDPTracker struct {
	conn *net.UDPConn

	// A UDP datagram is not a synchronisation edge: the serving goroutine
	// writes these while the test goroutine reads them once udpAnnounce
	// returns, and nothing orders the two. Guard them so -race can run.
	mu   sync.Mutex
	seen udpSeen

	done chan struct{}
}

// udpSeen is what the tracker was told, copied out under the lock.
type udpSeen struct {
	infoHash  [20]byte
	peerID    [20]byte
	port      uint16
	event     uint32
	left      uint64
	uploaded  uint64
	connects  int
	announces int
}

func (f *fakeUDPTracker) snapshot() udpSeen {
	f.mu.Lock()
	defer f.mu.Unlock()
	return f.seen
}

func startFakeUDPTracker(t *testing.T) *fakeUDPTracker {
	t.Helper()
	pc, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	f := &fakeUDPTracker{conn: pc, done: make(chan struct{})}
	go f.serve()
	t.Cleanup(func() { pc.Close(); <-f.done })
	return f
}

func (f *fakeUDPTracker) addr() string { return f.conn.LocalAddr().String() }

func (f *fakeUDPTracker) serve() {
	defer close(f.done)
	buf := make([]byte, 2048)
	for {
		n, peer, err := f.conn.ReadFromUDP(buf)
		if err != nil {
			return
		}
		if n < 16 {
			continue
		}
		action := binary.BigEndian.Uint32(buf[8:12])
		txn := binary.BigEndian.Uint32(buf[12:16])

		switch action {
		case udpActionConnect:
			if binary.BigEndian.Uint64(buf[0:8]) != udpProtocolID {
				continue // not a BEP 15 connect; ignore like a real tracker would
			}
			f.mu.Lock()
			f.seen.connects++
			f.mu.Unlock()
			resp := make([]byte, 16)
			binary.BigEndian.PutUint32(resp[0:4], udpActionConnect)
			binary.BigEndian.PutUint32(resp[4:8], txn)
			binary.BigEndian.PutUint64(resp[8:16], 0xCAFEBABEDEADBEEF)
			f.conn.WriteToUDP(resp, peer)

		case udpActionAnnounce:
			if n < 98 {
				continue
			}
			f.mu.Lock()
			f.seen.announces++
			copy(f.seen.infoHash[:], buf[16:36])
			copy(f.seen.peerID[:], buf[36:56])
			f.seen.left = binary.BigEndian.Uint64(buf[64:72])
			f.seen.uploaded = binary.BigEndian.Uint64(buf[72:80])
			f.seen.event = binary.BigEndian.Uint32(buf[80:84])
			f.seen.port = binary.BigEndian.Uint16(buf[96:98])
			f.mu.Unlock()

			resp := make([]byte, 20+12)
			binary.BigEndian.PutUint32(resp[0:4], udpActionAnnounce)
			binary.BigEndian.PutUint32(resp[4:8], txn)
			binary.BigEndian.PutUint32(resp[8:12], 1800) // interval
			binary.BigEndian.PutUint32(resp[12:16], 7)   // leechers
			binary.BigEndian.PutUint32(resp[16:20], 42)  // seeders
			copy(resp[20:26], []byte{1, 2, 3, 4, 0x1a, 0xe1})
			copy(resp[26:32], []byte{5, 6, 7, 8, 0x1a, 0xe2})
			f.conn.WriteToUDP(resp, peer)
		}
	}
}

const wireTestHash = "0123456789abcdef0123456789abcdef01234567"

func parseUDPTestURL(hostPort string) (*url.URL, error) {
	return url.Parse("udp://" + hostPort + "/announce")
}

func hexOf(b []byte) string { return hex.EncodeToString(b) }

func newWireTestAnnouncer() *trackerAnnouncer {
	return &trackerAnnouncer{peerID: "-HY3550-abcdefghijkl", port: 51413}
}

// The whole exchange, end to end: connect, announce, parse. This is what tells
// us the byte offsets are right, and a wrong offset here is invisible in a unit
// test of any single helper.
func TestUDPAnnounceWireRoundTrip(t *testing.T) {
	f := startFakeUDPTracker(t)
	udpConnCache = map[string]udpConnID{} // no id carried over from another test

	ta := newWireTestAnnouncer()
	u, err := parseUDPTestURL(f.addr())
	if err != nil {
		t.Fatal(err)
	}

	res, err := ta.udpAnnounce(u, wireTestHash, 4096, 2048, 1024, "started")
	if err != nil {
		t.Fatalf("udpAnnounce: %v", err)
	}

	if res.Interval != 1800 || res.Complete != 42 || res.Incomplete != 7 {
		t.Errorf("interval/complete/incomplete = %d/%d/%d, want 1800/42/7",
			res.Interval, res.Complete, res.Incomplete)
	}
	if len(res.Peers) != 2 {
		t.Fatalf("got %d peers, want 2 (%v)", len(res.Peers), res.Peers)
	}
	if res.Peers[0].IP != "1.2.3.4" || res.Peers[0].Port != 6881 {
		t.Errorf("peer 0 = %s:%d, want 1.2.3.4:6881", res.Peers[0].IP, res.Peers[0].Port)
	}

	// What the tracker actually received: these are the fields a private
	// tracker credits us on, so a silent offset error costs real ratio.
	seen := f.snapshot()
	if got := hexOf(seen.infoHash[:]); got != wireTestHash {
		t.Errorf("tracker saw info_hash %s, want %s", got, wireTestHash)
	}
	if string(seen.peerID[:]) != ta.peerID {
		t.Errorf("tracker saw peer_id %q, want %q", seen.peerID, ta.peerID)
	}
	if seen.port != 51413 {
		t.Errorf("tracker saw port %d, want 51413", seen.port)
	}
	if seen.event != 2 {
		t.Errorf("tracker saw event %d, want 2 (started)", seen.event)
	}
	if seen.left != 1024 || seen.uploaded != 4096 {
		t.Errorf("tracker saw left=%d uploaded=%d, want 1024/4096", seen.left, seen.uploaded)
	}
}

// The connection_id is cached: a second announce must not re-connect. At 100k
// torrents over a handful of trackers, one connect per announce would be a
// self-inflicted flood.
func TestUDPConnectionIDIsReused(t *testing.T) {
	f := startFakeUDPTracker(t)
	udpConnCache = map[string]udpConnID{}

	ta := newWireTestAnnouncer()
	u, err := parseUDPTestURL(f.addr())
	if err != nil {
		t.Fatal(err)
	}
	for i := 0; i < 3; i++ {
		if _, err := ta.udpAnnounce(u, wireTestHash, 0, 0, 0, ""); err != nil {
			t.Fatalf("announce %d: %v", i, err)
		}
	}
	seen := f.snapshot()
	if seen.connects != 1 {
		t.Errorf("tracker saw %d connects for 3 announces, want 1", seen.connects)
	}
	if seen.announces != 3 {
		t.Errorf("tracker saw %d announces, want 3", seen.announces)
	}
}

// A tracker that never answers must give up rather than wedge the announce
// loop. The retry ladder is ~14s, so this asserts it returns well inside the
// caller's patience and reports a failure.
func TestUDPAnnounceGivesUpOnSilentTracker(t *testing.T) {
	pc, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	defer pc.Close() // bound but never reads: packets are dropped, nothing replies
	udpConnCache = map[string]udpConnID{}

	ta := newWireTestAnnouncer()
	u, err := parseUDPTestURL(pc.LocalAddr().String())
	if err != nil {
		t.Fatal(err)
	}
	start := time.Now()
	if _, err := ta.udpAnnounce(u, wireTestHash, 0, 0, 0, ""); err == nil {
		t.Fatal("silent tracker reported success")
	}
	if el := time.Since(start); el > 25*time.Second {
		t.Errorf("took %s to give up; the announce loop would stall", el)
	}
}
