package engine

import (
	"fmt"
	"net"
	"testing"
)

// bencodeAnnounce builds a minimal tracker response carrying a compact v4 peer
// list and, optionally, a BEP-7 peers6 list. Keys are emitted in the sorted
// order bencode requires.
func bencodeAnnounce(peers4, peers6 []byte) []byte {
	out := "d8:completei5e10:incompletei2e"
	out += fmt.Sprintf("5:peers%d:%s", len(peers4), peers4)
	if peers6 != nil {
		out += fmt.Sprintf("6:peers6%d:%s", len(peers6), peers6)
	}
	out += "e"
	return []byte(out)
}

func compactV4(ip string, port int) []byte {
	b := net.ParseIP(ip).To4()
	return append(append([]byte{}, b...), byte(port>>8), byte(port&0xff))
}

func compactV6(ip string, port int) []byte {
	b := net.ParseIP(ip).To16()
	return append(append([]byte{}, b...), byte(port>>8), byte(port&0xff))
}

// The v4 list must survive untouched whether or not IPv6 is enabled: turning
// the setting on adds peers, it never changes how the old ones are read.
func TestParseTrackerResponseKeepsV4(t *testing.T) {
	data := bencodeAnnounce(compactV4("1.2.3.4", 6881), nil)
	for _, acceptV6 := range []bool{false, true} {
		res, err := parseTrackerResponse(data, acceptV6)
		if err != nil {
			t.Fatalf("acceptV6=%v: %v", acceptV6, err)
		}
		if len(res.Peers) != 1 || res.Peers[0].IP != "1.2.3.4" || res.Peers[0].Port != 6881 {
			t.Fatalf("acceptV6=%v: got %+v", acceptV6, res.Peers)
		}
		if res.Complete != 5 || res.Incomplete != 2 {
			t.Fatalf("acceptV6=%v: seeds/leechers got %d/%d", acceptV6, res.Complete, res.Incomplete)
		}
	}
}

// Off is the default, and it must stay byte-for-byte the old behaviour: a
// tracker that volunteers peers6 gets its v6 list dropped on the floor.
func TestParseTrackerResponseIgnoresPeers6WhenDisabled(t *testing.T) {
	data := bencodeAnnounce(compactV4("1.2.3.4", 6881), compactV6("2001:db8::1", 6882))
	res, err := parseTrackerResponse(data, false)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Peers) != 1 {
		t.Fatalf("expected the v6 peer to be ignored, got %+v", res.Peers)
	}
}

func TestParseTrackerResponseReadsPeers6WhenEnabled(t *testing.T) {
	data := bencodeAnnounce(compactV4("1.2.3.4", 6881), compactV6("2001:db8::1", 6882))
	res, err := parseTrackerResponse(data, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Peers) != 2 {
		t.Fatalf("expected v4 + v6, got %+v", res.Peers)
	}
	if res.Peers[1].IP != "2001:db8::1" || res.Peers[1].Port != 6882 {
		t.Fatalf("v6 peer mangled: %+v", res.Peers[1])
	}
}

// A v4-mapped entry inside peers6 is a v4 peer wearing a v6 hat. It has to come
// back as plain v4, otherwise the same machine counts twice depending on which
// list the tracker happened to put it in.
func TestParseTrackerResponseUnwrapsV4Mapped(t *testing.T) {
	data := bencodeAnnounce(compactV4("1.2.3.4", 6881), compactV6("::ffff:5.6.7.8", 6883))
	res, err := parseTrackerResponse(data, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Peers) != 2 {
		t.Fatalf("expected 2 peers, got %+v", res.Peers)
	}
	if res.Peers[1].IP != "5.6.7.8" {
		t.Fatalf("v4-mapped not unwrapped: %q", res.Peers[1].IP)
	}
}

// A truncated peers6 blob must not panic or invent a peer from the leftovers.
func TestParseTrackerResponseTruncatedPeers6(t *testing.T) {
	short := compactV6("2001:db8::1", 6882)[:11]
	data := bencodeAnnounce(compactV4("1.2.3.4", 6881), short)
	res, err := parseTrackerResponse(data, true)
	if err != nil {
		t.Fatal(err)
	}
	if len(res.Peers) != 1 {
		t.Fatalf("expected the truncated entry to be skipped, got %+v", res.Peers)
	}
}
