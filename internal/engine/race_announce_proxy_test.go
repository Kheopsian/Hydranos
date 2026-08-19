package engine

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// The race engine used to build its announcer from a bare port, which dropped
// every other announce-egress setting on the floor. A relay setup whose hoard
// announced through the proxy therefore had its race announces leave direct,
// and the tracker recorded the host's own address — once per restart, since
// the seed keepalive fires on the first watchdog tick after boot.
func TestRaceSessionAnnouncerRoutesThroughSOCKS5(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")

	var hits int
	tracker := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		hits++
		w.Write([]byte("d8:intervali1800e5:peers0:e"))
	}))
	defer tracker.Close()

	proxy := newFakeSOCKS5(t, "hydra", "s3cret")
	ta := newTrackerAnnouncerForSession(16171, &config.SessionConfig{
		ListenPort:    16171,
		AnnounceProxy: "socks5h://hydra:s3cret@" + proxy.addr(),
	}, "race")

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
		t.Fatalf("proxy saw %v, want one CONNECT to port %s — the race announce went direct", seen, u.Port())
	}
	if !proxy.sawAuth {
		t.Error("proxy credentials were never presented")
	}
	// The second half of the leak: with no proxy on the binding the dual-family
	// clients came alive and posted the SAME peer_id from the host's real
	// address, which a private tracker counts as a second seeding location.
	if ta.clientV4 != nil || ta.clientV6 != nil {
		t.Error("family-pinned clients built behind a proxy: the companion announce would go direct")
	}
	if !ta.proxied {
		t.Error("announcer not marked proxied, so the udp:// path would still leak")
	}
}

// announce_ip and enable_ipv6 reached the hoard announcer and not the race one
// for exactly the same reason, so they are worth pinning down here too.
func TestRaceSessionAnnouncerCarriesTheRestOfTheEgress(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	ta := newTrackerAnnouncerForSession(16171, &config.SessionConfig{
		ListenPort: 16171,
		AnnounceIP: "203.0.113.9",
		EnableIPv6: true,
	}, "race")
	if ta.publicIP != "203.0.113.9" {
		t.Errorf("publicIP = %q, want the configured announce_ip", ta.publicIP)
	}
	if !ta.enableIPv6 {
		t.Error("enableIPv6 = false, so BEP-7 peers6 would be dropped for the race")
	}
	if ta.port != 16171 {
		t.Errorf("port = %d, want 16171", ta.port)
	}
}

// Building an announcer mints a fresh peer_id. The keepalive built one per
// 30s tick, so the tracker saw a stream of distinct peers claiming the same
// port instead of one peer refreshing.
func TestRaceAnnouncerIsStableUntilThePortMoves(t *testing.T) {
	t.Setenv("TYPHON_ANNOUNCE_PROXY", "")
	e := NewRaceEngine(&config.SessionConfig{ListenPort: 16171}, nil, nil, t.TempDir())

	first := e.announcer()
	if e.announcer() != first {
		t.Fatal("announcer rebuilt on every call: peer_id changes under the tracker")
	}
	if first.port != 16171 {
		t.Fatalf("port = %d, want the configured 16171", first.port)
	}

	// A gluetun lease turning over moves the bound port; announcing the old one
	// publishes a port nobody answers on.
	e.livePort.Store(28001)
	moved := e.announcer()
	if moved == first {
		t.Fatal("announcer kept after the live port moved")
	}
	if moved.port != 28001 {
		t.Errorf("port = %d, want the live 28001", moved.port)
	}
}
