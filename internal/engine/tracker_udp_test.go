package engine

import (
	"encoding/binary"
	"net"
	"net/url"
	"testing"
)

// The peer list is the part a wrong guess corrupts silently: a bad stride still
// parses, it just yields nonsense addresses nobody can connect to.
func TestParseUDPPeersIPv4(t *testing.T) {
	b := []byte{
		1, 2, 3, 4, 0x1a, 0xe1, // 1.2.3.4:6881
		10, 0, 0, 1, 0x00, 0x50, // 10.0.0.1:80
	}
	peers := parseUDPPeers(b, false)
	if len(peers) != 2 {
		t.Fatalf("want 2 peers, got %d (%v)", len(peers), peers)
	}
	if peers[0].IP != "1.2.3.4" || peers[0].Port != 6881 {
		t.Errorf("peer 0 = %s:%d, want 1.2.3.4:6881", peers[0].IP, peers[0].Port)
	}
	if peers[1].IP != "10.0.0.1" || peers[1].Port != 80 {
		t.Errorf("peer 1 = %s:%d, want 10.0.0.1:80", peers[1].IP, peers[1].Port)
	}
}

// A tracker reached over IPv6 answers with 18-byte entries. BEP 15 does not
// describe this, and the length cannot tell us: see the ambiguity test below.
func TestParseUDPPeersIPv6(t *testing.T) {
	entry := make([]byte, 18)
	copy(entry[:16], net.ParseIP("2001:db8::1").To16())
	binary.BigEndian.PutUint16(entry[16:], 51413)
	peers := parseUDPPeers(entry, true)
	if len(peers) != 1 {
		t.Fatalf("want 1 peer, got %d", len(peers))
	}
	if peers[0].IP != "2001:db8::1" || peers[0].Port != 51413 {
		t.Errorf("got %s:%d, want 2001:db8::1:51413", peers[0].IP, peers[0].Port)
	}
}

// Regression: every 18-byte IPv6 list is also a whole number of 6-byte IPv4
// entries (36 bytes is 2 v6 peers or 6 v4 peers), so an implementation that
// picks the stride by divisibility silently decodes v6 peers as junk v4
// addresses and never connects to any of them. The family must come from the
// socket. This asserts the two readings really are both plausible by length,
// and that the flag is what decides.
func TestParseUDPPeersLengthIsAmbiguous(t *testing.T) {
	b := make([]byte, 36)
	copy(b[0:16], net.ParseIP("2001:db8::1").To16())
	binary.BigEndian.PutUint16(b[16:18], 6881)
	copy(b[18:34], net.ParseIP("2001:db8::2").To16())
	binary.BigEndian.PutUint16(b[34:36], 6882)

	if len(b)%6 != 0 || len(b)%18 != 0 {
		t.Fatal("test payload no longer exercises the ambiguity")
	}
	v6read := parseUDPPeers(b, true)
	if len(v6read) != 2 || v6read[0].IP != "2001:db8::1" || v6read[1].IP != "2001:db8::2" {
		t.Fatalf("v6 read: got %v, want the two 2001:db8:: peers", v6read)
	}
	// Read as IPv4 the same bytes still decode without error, into addresses
	// that were never in the response. That silent plausibility is the trap.
	v4read := parseUDPPeers(b, false)
	for _, p := range v4read {
		if p.IP == "2001:db8::1" || p.IP == "2001:db8::2" {
			t.Fatalf("v4 read recovered a real peer (%v); the payload no longer proves the ambiguity", p)
		}
	}
}

// Garbage must yield nothing rather than a partially-decoded peer.
func TestParseUDPPeersRejectsRagged(t *testing.T) {
	if p := parseUDPPeers([]byte{1, 2, 3}, false); p != nil {
		t.Errorf("ragged payload produced peers: %v", p)
	}
	if p := parseUDPPeers(nil, false); p != nil {
		t.Errorf("empty payload produced peers: %v", p)
	}
}

// A peer with port 0 is unusable; keeping it only wastes a dial slot.
func TestParseUDPPeersDropsZeroPort(t *testing.T) {
	b := []byte{1, 2, 3, 4, 0, 0}
	if p := parseUDPPeers(b, false); len(p) != 0 {
		t.Errorf("zero-port peer kept: %v", p)
	}
}

// Announcing while a SOCKS5 proxy is configured must fail closed. If this ever
// starts returning a result, UDP announces are leaving outside the tunnel and
// publishing the real address to the tracker.
func TestUDPAnnounceRefusesWhenProxyConfigured(t *testing.T) {
	// loadAnnounceProxy caches behind a sync.Once, so drive the package var
	// directly: consume the Once first, otherwise the next call re-reads an
	// empty env and clears what we set.
	announceProxyOnce.Do(func() {})
	saved := announceProxyURL
	announceProxyURL, _ = url.Parse("socks5h://user:pass@127.0.0.1:1080")
	defer func() { announceProxyURL = saved }()

	ta := &trackerAnnouncer{peerID: "-HY3550-abcdefghijkl", port: 6881}
	u, err := url.Parse("udp://tracker.example.org:6969/announce")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := ta.udpAnnounce(u, "0123456789abcdef0123456789abcdef01234567", 0, 0, 0, ""); err == nil {
		t.Fatal("udpAnnounce succeeded with a SOCKS5 proxy configured; it must refuse")
	}
}

// The event word must map to the exact code BEP 15 defines; an off-by-one here
// tells the tracker we stopped when we started.
func TestUDPEventCodes(t *testing.T) {
	for word, want := range map[string]uint32{"": 0, "completed": 1, "started": 2, "stopped": 3} {
		if got := udpEventCode[word]; got != want {
			t.Errorf("event %q = %d, want %d", word, got, want)
		}
	}
}
