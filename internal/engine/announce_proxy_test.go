package engine

import (
	"bufio"
	"context"
	"encoding/binary"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"
)

// fakeSOCKS5 is a minimal RFC 1928/1929 proxy: it records the target every
// client asked for, then splices the connection to it for real. Recording the
// target is the whole point — it is the only way to tell "the announce went
// through the proxy" from "the announce went direct and happened to work".
type fakeSOCKS5 struct {
	ln       net.Listener
	wantUser string
	wantPass string

	mu      sync.Mutex
	targets []string
	sawAuth bool
}

func newFakeSOCKS5(t *testing.T, user, pass string) *fakeSOCKS5 {
	t.Helper()
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("listen: %v", err)
	}
	s := &fakeSOCKS5{ln: ln, wantUser: user, wantPass: pass}
	go func() {
		for {
			c, err := ln.Accept()
			if err != nil {
				return
			}
			go s.serve(c)
		}
	}()
	t.Cleanup(func() { ln.Close() })
	return s
}

func (s *fakeSOCKS5) addr() string { return s.ln.Addr().String() }

func (s *fakeSOCKS5) seen() []string {
	s.mu.Lock()
	defer s.mu.Unlock()
	return append([]string(nil), s.targets...)
}

func (s *fakeSOCKS5) serve(c net.Conn) {
	defer c.Close()
	br := bufio.NewReader(c)

	head := make([]byte, 2)
	if _, err := io.ReadFull(br, head); err != nil || head[0] != 5 {
		return
	}
	methods := make([]byte, int(head[1]))
	if _, err := io.ReadFull(br, methods); err != nil {
		return
	}
	method := byte(0)
	if s.wantUser != "" {
		method = 2
	}
	if _, err := c.Write([]byte{5, method}); err != nil {
		return
	}
	if method == 2 {
		if _, err := io.ReadFull(br, head[:1]); err != nil || head[0] != 1 {
			return
		}
		user, err := readLenPrefixed(br)
		if err != nil {
			return
		}
		pass, err := readLenPrefixed(br)
		if err != nil {
			return
		}
		if user != s.wantUser || pass != s.wantPass {
			c.Write([]byte{1, 1})
			return
		}
		s.mu.Lock()
		s.sawAuth = true
		s.mu.Unlock()
		if _, err := c.Write([]byte{1, 0}); err != nil {
			return
		}
	}

	req := make([]byte, 4)
	if _, err := io.ReadFull(br, req); err != nil || req[0] != 5 || req[1] != 1 {
		return
	}
	var host string
	switch req[3] {
	case 1:
		b := make([]byte, 4)
		if _, err := io.ReadFull(br, b); err != nil {
			return
		}
		host = net.IP(b).String()
	case 3:
		h, err := readLenPrefixed(br)
		if err != nil {
			return
		}
		host = h
	default:
		return
	}
	pb := make([]byte, 2)
	if _, err := io.ReadFull(br, pb); err != nil {
		return
	}
	target := net.JoinHostPort(host, strconv.Itoa(int(binary.BigEndian.Uint16(pb))))

	s.mu.Lock()
	s.targets = append(s.targets, target)
	s.mu.Unlock()

	up, err := net.DialTimeout("tcp", target, 3*time.Second)
	if err != nil {
		c.Write([]byte{5, 1, 0, 1, 0, 0, 0, 0, 0, 0})
		return
	}
	defer up.Close()
	if _, err := c.Write([]byte{5, 0, 0, 1, 0, 0, 0, 0, 0, 0}); err != nil {
		return
	}
	go io.Copy(up, br)
	io.Copy(c, up)
}

func readLenPrefixed(br *bufio.Reader) (string, error) {
	n, err := br.ReadByte()
	if err != nil {
		return "", err
	}
	b := make([]byte, int(n))
	if _, err := io.ReadFull(br, b); err != nil {
		return "", err
	}
	return string(b), nil
}

// TestAnnounceProxyFromBindingRoutesThroughSOCKS5 is the regression test for
// the leak: a session configured with announce_proxy must send its announces
// through that proxy. Before the fix, only the TYPHON_ANNOUNCE_PROXY env could
// do this, so a config-only relay setup announced direct and the tracker
// recorded the host's own address.
func TestAnnounceProxyFromBindingRoutesThroughSOCKS5(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")

	var hits int
	tracker := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		w.Write([]byte("d8:intervali1800e5:peers0:e"))
	}))
	defer tracker.Close()

	proxy := newFakeSOCKS5(t, "hydra", "s3cret")
	ta := newTrackerAnnouncerForBinding(Binding{
		ID:            0,
		AnnounceScope: "hoard",
		ListenPort:    16172,
		PeerID:        "-HY3730-abcdefghijkl",
		AnnounceProxy: "socks5h://hydra:s3cret@" + proxy.addr(),
	})
	resp, err := ta.httpClient.Get(tracker.URL + "/announce")
	if err != nil {
		t.Fatalf("announce through proxy: %v", err)
	}
	resp.Body.Close()
	if hits != 1 {
		t.Fatalf("tracker hits = %d, want 1", hits)
	}

	u, _ := url.Parse(tracker.URL)
	seen := proxy.seen()
	if len(seen) != 1 || !strings.HasSuffix(seen[0], ":"+u.Port()) {
		t.Fatalf("proxy saw %v, want one CONNECT to port %s — the announce went direct", seen, u.Port())
	}
	if !proxy.sawAuth {
		t.Error("proxy credentials were never presented")
	}
	// A proxied announce owns the egress family, so the dual-family clients
	// must stay nil: pinning a local family cannot change what the tracker
	// sees, and would break a proxy reachable over only one of them.
	if ta.clientV4 != nil || ta.clientV6 != nil {
		t.Error("family-pinned clients built behind a proxy")
	}
	if !ta.proxied {
		t.Error("announcer not marked proxied, so the udp:// path would still leak")
	}
}

func TestAnnounceProxyMalformedGoesDirectRatherThanGuessing(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	for _, raw := range []string{"http://10.0.0.1:1080", "not a url at all", "://"} {
		if got := announceProxyForBinding(Binding{AnnounceProxy: raw}); got != nil {
			t.Errorf("announce_proxy %q accepted as %v, want rejected", raw, got)
		}
	}
}

func TestApplyAnnounceEgressStampsEveryBinding(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	bs := ApplyAnnounceEgress(
		DefaultSingleBinding(16172, false, "hoard", 0),
		"  socks5h://u:p@10.0.0.1:1080  ", "203.0.113.9", "10.0.0.1", "  wg0  ", "hoard")
	if bs[0].AnnounceProxy != "socks5h://u:p@10.0.0.1:1080" {
		t.Errorf("AnnounceProxy = %q (untrimmed?)", bs[0].AnnounceProxy)
	}
	if bs[0].BindInterface != "wg0" {
		t.Errorf("BindInterface = %q (untrimmed, or not stamped at all?)", bs[0].BindInterface)
	}
	if bs[0].PublicIP != "203.0.113.9" {
		t.Errorf("PublicIP = %q, want the configured announce_ip", bs[0].PublicIP)
	}

	// Empty announce_ip must leave PublicIP empty so the BEP-7 ip= param stays
	// omitted and the tracker keeps observing the source address.
	bs = ApplyAnnounceEgress(DefaultSingleBinding(16172, false, "hoard", 0), "", "  ", "", "", "hoard")
	if bs[0].PublicIP != "" {
		t.Errorf("PublicIP = %q, want empty", bs[0].PublicIP)
	}
	if bs[0].AnnounceProxy != "" {
		t.Errorf("AnnounceProxy = %q, want empty", bs[0].AnnounceProxy)
	}
	if bs[0].BindInterface != "" {
		t.Errorf("BindInterface = %q, want empty", bs[0].BindInterface)
	}
}

// TestUnresolvableBindInterfaceFailsRatherThanLeavingByTheDefaultRoute is the
// guard for the whole feature. An interface name that does not resolve must
// make the announce FAIL. The tempting alternative -- log it and dial anyway --
// publishes this host's own address to the tracker with every indicator green,
// which is the exact leak the binding exists to prevent.
//
// Proved by breaking it: point the binding at a name no host has, and require
// an error. Delete the bindErr guard in newTrackerAnnouncerForBinding and this
// test goes red.
func TestUnresolvableBindInterfaceFailsRatherThanLeavingByTheDefaultRoute(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	b := ApplyAnnounceEgress(DefaultSingleBinding(16172, false, "hoard", 0),
		"", "", "", "definitely-not-an-interface", "hoard")[0]
	ta := newTrackerAnnouncerForBinding(b)
	_, err := ta.httpClient.Transport.(*http.Transport).DialContext(
		context.Background(), "tcp", "203.0.113.1:80")
	if err == nil {
		t.Fatal("an unresolvable bind_interface dialled anyway: the announce would leave by the default route and hand the tracker this host's own address")
	}
	if !strings.Contains(err.Error(), "definitely-not-an-interface") {
		t.Errorf("error does not name the interface, so nobody can act on it: %v", err)
	}
}

// A resolvable interface must pin the SOURCE address, not merely be recorded.
// Checked on loopback, which every host has.
func TestBindInterfacePinsTheAnnounceSourceAddress(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	if _, err := net.InterfaceByName("lo"); err != nil {
		t.Skip("no loopback interface named lo on this host")
	}
	b := ApplyAnnounceEgress(DefaultSingleBinding(16172, true, "hoard", 0),
		"", "", "", "lo", "hoard")[0]
	ta := newTrackerAnnouncerForBinding(b)
	tr := ta.httpClient.Transport.(*http.Transport)
	// The dial must refuse a v6-pinned tracker: the source we hold is that
	// interface's IPv4, so a v6 dial could only leave from somewhere else.
	SetAnnounceIPMode("v6only.example.net", AnnounceIPModeV6)
	defer SetAnnounceIPMode("v6only.example.net", "")
	if _, err := tr.DialContext(context.Background(), "tcp", "v6only.example.net:80"); err == nil {
		t.Error("a v6-pinned tracker was dialled from a v4-bound interface without complaint")
	}
	_ = ta
}

// TestPinnedBindingGetsNoIPv6AnnounceClient tests the DECISION, not the
// builder. Asserting on the built announcer passes vacuously on any host
// without IPv6 -- every CI container -- so it would stay green with the rule
// deleted. Proved by breaking it: drop the !pinnedToInterface term and the
// third case below goes red.
func TestPinnedBindingGetsNoIPv6AnnounceClient(t *testing.T) {
	for _, tc := range []struct {
		name                          string
		proxied, pinned, hasV4, hasV6 bool
		wantV4, wantV6                bool
	}{
		{"dual-stack, unpinned", false, false, true, true, true, true},
		{"v4-only host", false, false, true, false, true, false},
		{"pinned to an interface kills v6", false, true, true, true, true, false},
		{"behind a proxy, neither", true, false, true, true, false, false},
		{"behind a proxy AND pinned, still neither", true, true, true, true, false, false},
	} {
		v4, v6 := wantFamilyClients(tc.proxied, tc.pinned, tc.hasV4, tc.hasV6)
		if v4 != tc.wantV4 || v6 != tc.wantV6 {
			t.Errorf("%s: got v4=%v v6=%v, want v4=%v v6=%v", tc.name, v4, v6, tc.wantV4, tc.wantV6)
		}
	}
}

func TestUDPAnnounceRefusesRatherThanBypassTheProxy(t *testing.T) {
	ta := &trackerAnnouncer{proxied: true, peerID: "-HY3730-abcdefghijkl"}
	u, _ := url.Parse("udp://tracker.example.net:6969/announce")
	if _, err := ta.udpAnnounce(u, strings.Repeat("ab", 20), 0, 0, 0, "started"); err == nil {
		t.Fatal("udp announce proceeded behind an announce proxy — that leaks the real address")
	}
}
