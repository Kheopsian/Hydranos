package engine

import (
	"context"
	"net"
	"net/http"
	"testing"
)

func announceDialer(t *testing.T, enableIPv6 bool) func(context.Context, string, string) (net.Conn, error) {
	t.Helper()
	ta := newTrackerAnnouncerForBinding(Binding{EnableIPv6: enableIPv6})
	tr, ok := ta.httpClient.Transport.(*http.Transport)
	if !ok {
		t.Fatalf("announce transport is %T, cannot inspect its dialer", ta.httpClient.Transport)
	}
	return tr.DialContext
}

// The setting says "IPv4 only" when it is off, and it used to govern only the
// listener, the tracker's peers6 list and the self-dial filter. The announce
// itself went out on a plain "tcp" dial, which follows RFC 6724 and prefers
// IPv6 wherever the host has it -- so the tracker registered a v6 address for
// someone who had asked for none, with nothing in the interface to explain it.
func TestAnnounceIsPinnedToIPv4WhenIPv6IsOff(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "") // a proxy owns the egress family, skip that path

	ln, err := net.Listen("tcp", "[::1]:0")
	if err != nil {
		t.Skip("no usable IPv6 loopback here")
	}
	defer ln.Close()
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			c.Close()
		}
	}()
	v6addr := ln.Addr().String()

	// Off: the v6 destination must be unreachable, because the dial is pinned
	// to IPv4 rather than merely preferring it.
	if conn, err := announceDialer(t, false)(context.Background(), "tcp", v6addr); err == nil {
		conn.Close()
		t.Fatal("the announce reached an IPv6 address with enable_ipv6 off")
	}

	// On: nothing is pinned, and the same address answers. Without this half
	// the test above would pass just as well against a dialer that is broken
	// for everyone.
	conn, err := announceDialer(t, true)(context.Background(), "tcp", v6addr)
	if err != nil {
		t.Fatalf("enable_ipv6 on, but the announce could not use IPv6: %v", err)
	}
	conn.Close()
}

// IPv4 is forced by narrowing the network name, so the mapping is the whole
// mechanism. Each case carries why it matters: a bare "tcp" is the one the
// kernel resolves to v6, an explicit "tcp6" is exactly what the setting
// forbids, and anything that is not an IP family has to pass through untouched
// rather than be guessed at.
func TestIPv4NetworkNarrowsEveryFamilyItShould(t *testing.T) {
	for _, c := range []struct{ in, want, why string }{
		{"tcp", "tcp4", "the unqualified form is the one the kernel resolves to v6"},
		{"tcp6", "tcp4", "an explicit v6 dial is exactly what the setting forbids"},
		{"tcp4", "tcp4", "already narrowed"},
		{"udp", "udp4", "same reasoning as tcp, for anything dialling udp"},
		{"udp6", "udp4", "explicit v6 again"},
		{"udp4", "udp4", "already narrowed"},
		{"unix", "unix", "not an IP family: passed through rather than guessed at"},
		{"weirdnet", "weirdnet", "an unknown network must never be rewritten"},
		{"", "", "the empty network is not ours to interpret"},
	} {
		if got := ipv4Network(c.in); got != c.want {
			t.Errorf("ipv4Network(%q) = %q, want %q -- %s", c.in, got, c.want, c.why)
		}
	}
}

// A host with no IPv4 cannot honour the pin, and the boot warning that says so
// depends on this answering honestly. A machine running the tests has a
// loopback at minimum, so the only assertion that holds everywhere is that it
// does not crash and agrees with a direct look at the interfaces.
func TestHostHasIPv4MatchesTheInterfaces(t *testing.T) {
	addrs, err := net.InterfaceAddrs()
	if err != nil {
		t.Skip("cannot enumerate interfaces here")
	}
	want := false
	for _, a := range addrs {
		if ipn, ok := a.(*net.IPNet); ok && !ipn.IP.IsLoopback() && ipn.IP.To4() != nil {
			want = true
			break
		}
	}
	if got := HostHasIPv4(); got != want {
		t.Errorf("HostHasIPv4() = %v, want %v", got, want)
	}
}
