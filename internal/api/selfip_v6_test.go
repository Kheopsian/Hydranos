package api

import (
	"net"
	"testing"
)

// The self-dial filter has to hold every address a swarm could hand back to us.
// Production ran 8348 self-connections to a Docker bridge address because only
// the public one was in the list.
func TestIsSelfCandidateIPv6(t *testing.T) {
	cases := []struct {
		in   string
		want bool
		why  string
	}{
		{"2001:db8:1:2240::1", true, "Docker bridge GUA, the address that caused the incident"},
		{"2001:db8:1:2200::200", true, "our stable LAN address"},
		{"fd99::1", true, "ULA: dialling ourselves is useless whatever the scope"},
		{"fe80::3efd:feff:fe1d:7fc2", false, "link-local cannot reach us from a swarm"},
		{"::1", false, "loopback"},
		{"192.168.99.200", false, "v4 is covered by the public-IP lookup"},
		{"203.0.113.10", false, "public v4, same"},
		{"::ffff:1.2.3.4", false, "v4-mapped is a v4 address wearing a v6 hat"},
	}
	for _, c := range cases {
		ip := net.ParseIP(c.in)
		if ip == nil {
			t.Fatalf("%s: unparseable", c.in)
		}
		if got := isSelfCandidateIPv6(ip); got != c.want {
			t.Errorf("isSelfCandidateIPv6(%s) = %v, want %v (%s)", c.in, got, c.want, c.why)
		}
	}
	if isSelfCandidateIPv6(nil) {
		t.Error("nil must not be treated as one of our addresses")
	}
}

func TestDedupeStrings(t *testing.T) {
	in := []string{"a", "b", "a", "c", "b"}
	got := dedupeStrings(in)
	if len(got) != 3 || got[0] != "a" || got[1] != "b" || got[2] != "c" {
		t.Fatalf("got %v, want [a b c] in first-seen order", got)
	}
	if n := len(dedupeStrings(nil)); n != 0 {
		t.Fatalf("nil input: got %d entries", n)
	}
}
