package api

import (
	"github.com/Kheopsian/hydra/internal/config"
	"strconv"
	"strings"
	"testing"
)

const netSample = `[daemon]
api_port = 8199

[race]
listen_port = 16171
enable_ipv6 = false

[hoard]
listen_port = 16172
enable_ipv6 = false
`

// apply runs one mode through the same path the handler uses, and hands back
// the reparsed config — testing the keys in isolation would miss the part that
// actually matters, which is what survives in the file.
func apply(t *testing.T, doc, mode string, f netModeFields) (map[string]interface{}, string) {
	t.Helper()
	out := doc
	var err error
	for _, e := range []struct {
		section     string
		listenPort  int
		proxyV2Port int
	}{
		{"race", f.RaceListenPort, f.RaceProxyV2Port},
		{"hoard", f.HoardListenPort, f.HoardProxyV2Port},
	} {
		out, err = config.SetTOMLTable(out, e.section, netModeKeys(mode, f, e.listenPort, e.proxyV2Port, e.section))
		if err != nil {
			t.Fatalf("SetTOMLTable(%s): %v", e.section, err)
		}
	}
	if err := config.ValidateTyped([]byte(out)); err != nil {
		t.Fatalf("mode %s produced a config the daemon would refuse: %v", mode, err)
	}
	m, err := config.ParseTOMLMap([]byte(out))
	if err != nil {
		t.Fatalf("reparse: %v", err)
	}
	return m, out
}

func socksFields() netModeFields {
	return netModeFields{
		RaceListenPort: 16171, HoardListenPort: 16172,
		Socks5Host: "10.0.0.1", Socks5Port: 1080,
		Socks5User: "hydra", Socks5Pass: "p@ss word",
	}
}

// TestSocks5ModeWiresBothPaths is the reason this tab exists: the operator
// enters one proxy and BOTH the peer dials and the announces must follow it.
// Setting only the first is the leak that started all this.
func TestSocks5ModeWiresBothPaths(t *testing.T) {
	m, _ := apply(t, netSample, netModeSocks5, socksFields())
	for _, sec := range []string{"race", "hoard"} {
		s := sectionOf(m, sec)
		if got := tomlStr(s, "socks5_outbound_host"); got != "10.0.0.1" {
			t.Errorf("[%s] socks5_outbound_host = %q", sec, got)
		}
		ap := tomlStr(s, "announce_proxy")
		if !strings.HasPrefix(ap, "socks5h://") || !strings.Contains(ap, "10.0.0.1:1080") {
			t.Errorf("[%s] announce_proxy = %q, want the same proxy", sec, ap)
		}
		// The password has a space and an @: unescaped, the URL would either be
		// refused or parsed with the wrong host.
		if !strings.Contains(ap, "p%40ss%20word") {
			t.Errorf("[%s] announce_proxy credentials not escaped: %q", sec, ap)
		}
	}
	if detectNetMode(sectionOf(m, "race"), sectionOf(m, "hoard")) != netModeSocks5 {
		t.Error("mode not detected back as socks5")
	}
}

// TestSwitchingModeClearsTheOldOne — the half-abandoned setup is the failure
// this tab is meant to make impossible.
func TestSwitchingModeClearsTheOldOne(t *testing.T) {
	f := socksFields()
	f.RaceProxyV2Port, f.HoardProxyV2Port = 16271, 16272
	f.ProxyV2Trusted = []string{"203.0.113.7"}
	_, doc := apply(t, netSample, netModeProxyV2, f)

	m, _ := apply(t, doc, netModeDirect, netModeFields{RaceListenPort: 16171, HoardListenPort: 16172})
	for _, sec := range []string{"race", "hoard"} {
		s := sectionOf(m, sec)
		for _, k := range []string{"socks5_outbound_host", "socks5_outbound_user", "socks5_outbound_pass", "announce_proxy", "bind_interface"} {
			if v := tomlStr(s, k); v != "" {
				t.Errorf("[%s] %s = %q after switching to direct, want cleared", sec, k, v)
			}
		}
		if v := tomlInt(s, "socks5_outbound_port"); v != 0 {
			t.Errorf("[%s] socks5_outbound_port = %d, want 0", sec, v)
		}
		if v := tomlInt(s, "listen_port_proxy_v2"); v != 0 {
			t.Errorf("[%s] listen_port_proxy_v2 = %d, want 0", sec, v)
		}
		if v := tomlStrList(s, "proxy_v2_trusted_sources"); len(v) != 0 {
			t.Errorf("[%s] proxy_v2_trusted_sources = %v, want empty", sec, v)
		}
	}
	if got := detectNetMode(sectionOf(m, "race"), sectionOf(m, "hoard")); got != netModeDirect {
		t.Errorf("mode = %q after clearing, want direct", got)
	}
}

func TestModeDetection(t *testing.T) {
	cases := []struct {
		name string
		mode string
		f    netModeFields
	}{
		{"direct", netModeDirect, netModeFields{RaceListenPort: 16171, HoardListenPort: 16172}},
		{"socks5", netModeSocks5, socksFields()},
	}
	for _, tc := range cases {
		m, _ := apply(t, netSample, tc.mode, tc.f)
		if got := detectNetMode(sectionOf(m, "race"), sectionOf(m, "hoard")); got != tc.mode {
			t.Errorf("%s: detected %q", tc.name, got)
		}
	}
	// proxy_v2 outranks socks5: both sets of keys are present in that mode, and
	// the more specific one has to win or the tab would show the wrong form.
	f := socksFields()
	f.RaceProxyV2Port, f.HoardProxyV2Port = 16271, 16272
	f.ProxyV2Trusted = []string{"203.0.113.7"}
	m, _ := apply(t, netSample, netModeProxyV2, f)
	if got := detectNetMode(sectionOf(m, "race"), sectionOf(m, "hoard")); got != netModeProxyV2 {
		t.Errorf("proxy_v2 detected as %q", got)
	}
}

func TestValidateRefusesBrokenSetups(t *testing.T) {
	base := socksFields()
	samePort := base
	samePort.HoardListenPort = samePort.RaceListenPort

	noHost := base
	noHost.Socks5Host = ""

	halfAuth := base
	halfAuth.Socks5Pass = ""

	pv2NoTrust := base
	pv2NoTrust.RaceProxyV2Port = 16271

	pv2ClashingPort := base
	pv2ClashingPort.RaceProxyV2Port = 16172 // = the hoard listen port
	pv2ClashingPort.ProxyV2Trusted = []string{"203.0.113.7"}

	pv2BadSource := base
	pv2BadSource.RaceProxyV2Port = 16271
	pv2BadSource.ProxyV2Trusted = []string{"not-an-address"}

	for _, tc := range []struct {
		name string
		mode string
		f    netModeFields
	}{
		{"both engines on one port", netModeSocks5, samePort},
		{"proxy mode without a host", netModeSocks5, noHost},
		{"username without password", netModeSocks5, halfAuth},
		{"proxy-v2 with no trusted source", netModeProxyV2, pv2NoTrust},
		{"proxy-v2 port already a listen port", netModeProxyV2, pv2ClashingPort},
		{"proxy-v2 trusted source is not an address", netModeProxyV2, pv2BadSource},
		{"gluetun with no interface on either engine", netModeGluetun, netModeFields{RaceListenPort: 16171, HoardListenPort: 16172}},
		{"an interface no host has", netModeDirect, netModeFields{RaceListenPort: 16171, HoardListenPort: 16172, RaceBindInterface: "definitely-not-an-interface"}},
		{"unknown mode", "tunnelvision", base},
	} {
		if err := validateNetMode(tc.mode, tc.f); err == nil {
			t.Errorf("%s: accepted, want refused", tc.name)
		}
	}

	if err := validateNetMode(netModeDirect, netModeFields{RaceListenPort: 16171, HoardListenPort: 16172}); err != nil {
		t.Errorf("plain direct setup refused: %v", err)
	}
	if err := validateNetMode(netModeSocks5, base); err != nil {
		t.Errorf("valid socks5 setup refused: %v", err)
	}
}

func TestWarningsSayWhatTheModeCosts(t *testing.T) {
	w := modeWarnings(netModeSocks5, socksFields(), nil)
	if len(w) == 0 || !strings.Contains(strings.ToLower(strings.Join(w, " ")), "udp") {
		t.Errorf("no UDP-tracker warning in proxied mode: %v", w)
	}
	if got := modeWarnings(netModeDirect, netModeFields{}, nil); len(got) != 0 {
		t.Errorf("direct mode warns about %v, want nothing", got)
	}
	// An env var that overrides the page must be surfaced, not silently obeyed.
	env := []envOverride{{Name: "TYPHON_ANNOUNCE_PROXY", Value: "socks5h://x:***@10.0.0.1:1080"}}
	got := modeWarnings(netModeDirect, netModeFields{}, env)
	if len(got) != 1 || !strings.Contains(got[0], "TYPHON_ANNOUNCE_PROXY") {
		t.Errorf("env override not reported in direct mode: %v", got)
	}
}

func TestMaskProxyURLKeepsTheHostAndHidesThePassword(t *testing.T) {
	got := maskProxyURL("socks5h://hydra:s3cret@10.0.0.1:1080")
	if strings.Contains(got, "s3cret") {
		t.Errorf("password leaked in %q", got)
	}
	if !strings.Contains(got, "10.0.0.1:1080") || !strings.Contains(got, "hydra") {
		t.Errorf("masked too much: %q", got)
	}
}

// TestSocks5ModeSaysItIsNotReachable — a plain SOCKS5 proxy forwards outgoing
// connections only. Presenting that mode as a finished setup hides the fact
// that the announced address answers nobody.
func TestSocks5ModeSaysItIsNotReachable(t *testing.T) {
	joined := strings.ToLower(strings.Join(modeWarnings(netModeSocks5, socksFields(), nil), " "))
	if !strings.Contains(joined, "outgoing") || !strings.Contains(joined, "reach you") {
		t.Errorf("socks5 mode does not warn about inbound: %q", joined)
	}
	// The relay mode exists precisely to fix it, so it must NOT carry the same
	// warning or the two modes become indistinguishable.
	f := socksFields()
	f.RaceProxyV2Port, f.HoardProxyV2Port = 16271, 16272
	f.ProxyV2Trusted = []string{"203.0.113.7"}
	relay := strings.ToLower(strings.Join(modeWarnings(netModeProxyV2, f, nil), " "))
	if strings.Contains(relay, "nobody can reach you") {
		t.Errorf("relay mode wrongly warns it is unreachable: %q", relay)
	}
}

// TestVPNModeFlagsANonTunnelInterface — the picker lists every interface on the
// host, so the ordinary one is often the first that looks plausible. Bound to
// it, peer connections leave outside the tunnel or go nowhere.
func TestVPNModeFlagsANonTunnelInterface(t *testing.T) {
	f := netModeFields{RaceListenPort: 16171, HoardListenPort: 16172, RaceBindInterface: "eth0", HoardBindInterface: "eth0"}
	joined := strings.Join(modeWarnings(netModeGluetun, f, nil), " ")
	if !strings.Contains(joined, "eth0") || !strings.Contains(joined, "does not look like a VPN tunnel") {
		t.Errorf("eth0 accepted silently in gluetun mode: %q", joined)
	}
	for _, name := range []string{"tun1", "wg0", "tap0", "tailscale0", "ppp0"} {
		f.RaceBindInterface, f.HoardBindInterface = name, name
		if got := strings.Join(modeWarnings(netModeGluetun, f, nil), " "); strings.Contains(got, "does not look like") {
			t.Errorf("%s wrongly flagged: %q", name, got)
		}
	}
	// Advisory only: a name we do not recognise must still be saveable, since
	// the set of VPN clients is open-ended. "lo" is used because validation now
	// insists the interface EXISTS, which is a separate rule from the heuristic.
	f.RaceBindInterface, f.HoardBindInterface = "lo", "lo"
	if err := validateNetMode(netModeGluetun, f); err != nil && strings.Contains(err.Error(), "tunnel") {
		t.Errorf("the naming heuristic blocks a save: %v", err)
	}
}

// TestDirectModeCarriesOneInterfacePerEngine — the point of the whole change.
// Two engines, two tunnels, and each engine's keys written into its own
// section. A single shared value here would silently put both engines on one
// tunnel while the page showed two.
func TestDirectModeCarriesOneInterfacePerEngine(t *testing.T) {
	f := netModeFields{
		RaceListenPort: 16171, HoardListenPort: 16172,
		RaceBindInterface: "wg-race", HoardBindInterface: "wg-hoard",
	}
	for _, tc := range []struct{ scope, want string }{{"race", "wg-race"}, {"hoard", "wg-hoard"}} {
		got := ""
		for _, kv := range netModeKeys(netModeDirect, f, 16171, 0, tc.scope) {
			if kv[0] == "bind_interface" {
				got = kv[1]
			}
		}
		if got != strconv.Quote(tc.want) {
			t.Errorf("[%s] bind_interface = %s, want %q", tc.scope, got, tc.want)
		}
	}
}

// TestModeIsDeducedFromGluetunNotFromAnInterface — bind_interface used to BE
// the mode. Now that direct carries it too, keying on it would read every
// bare-metal WireGuard host as gluetun and offer it a control-server URL it has
// no use for.
func TestModeIsDeducedFromGluetunNotFromAnInterface(t *testing.T) {
	iface := map[string]interface{}{"bind_interface": "wg0"}
	if got := detectNetMode(iface, map[string]interface{}{}); got != netModeDirect {
		t.Errorf("a bare interface reads as %q, want %q", got, netModeDirect)
	}
	glue := map[string]interface{}{"bind_interface": "tun0", "gluetun_port_forward": true}
	if got := detectNetMode(glue, map[string]interface{}{}); got != netModeGluetun {
		t.Errorf("a gluetun setup reads as %q, want %q", got, netModeGluetun)
	}
	// A control-server URL alone is enough: the flag can be off while the
	// operator is still pointed at gluetun.
	url := map[string]interface{}{"gluetun_url": "http://127.0.0.1:8000"}
	if got := detectNetMode(map[string]interface{}{}, url); got != netModeGluetun {
		t.Errorf("a gluetun URL reads as %q, want %q", got, netModeGluetun)
	}
	// And a proxy still outranks both, or the announce leak comes back.
	socks := map[string]interface{}{"socks5_outbound_host": "10.0.0.1", "bind_interface": "wg0"}
	if got := detectNetMode(socks, map[string]interface{}{}); got != netModeSocks5 {
		t.Errorf("a proxy setup reads as %q, want %q", got, netModeSocks5)
	}
}

// TestHalfBoundDirectSetupIsCalledOut — one engine in a tunnel and the other on
// the default route is the new failure this feature makes reachable, and the
// one that looks healthiest: the page shows a tunnel, and it is real, for half
// the traffic.
func TestHalfBoundDirectSetupIsCalledOut(t *testing.T) {
	f := netModeFields{RaceListenPort: 16171, HoardListenPort: 16172, RaceBindInterface: "wg0"}
	joined := strings.Join(modeWarnings(netModeDirect, f, nil), " ")
	if !strings.Contains(joined, "hoard") || !strings.Contains(joined, "default route") {
		t.Errorf("a half-bound setup passes without comment: %q", joined)
	}
	// Both bound, or neither, is a deliberate setup and must stay quiet.
	f.HoardBindInterface = "wg1"
	if got := modeWarnings(netModeDirect, f, nil); len(got) != 0 {
		t.Errorf("two bound engines warned about %v, want nothing", got)
	}
	if got := modeWarnings(netModeDirect, netModeFields{}, nil); len(got) != 0 {
		t.Errorf("an unbound direct setup warned about %v, want nothing", got)
	}
}

// The forwarded port goes to exactly one engine, and switching which one has to
// take it away from the other in the same save. Leaving both flags true is the
// failure this whole choice exists to prevent: the second engine to start would
// fail to bind, and the tab would still look correctly configured.
func TestGluetunPortGoesToOneEngineOnly(t *testing.T) {
	base := netModeFields{
		RaceListenPort:     16171,
		HoardListenPort:    16172,
		RaceBindInterface:  "lo",
		HoardBindInterface: "lo",
		GluetunPort:        true,
		GluetunURL:         "http://127.0.0.1:8000",
	}
	for _, tc := range []struct{ pick, on, off string }{
		{"", "hoard", "race"}, // configs written before the choice existed
		{"hoard", "hoard", "race"},
		{"race", "race", "hoard"},
	} {
		f := base
		f.GluetunEngine = tc.pick
		m, _ := apply(t, netSample, netModeGluetun, f)
		if !tomlBool(sectionOf(m, tc.on), "gluetun_port_forward") {
			t.Fatalf("pick %q: %s does not follow the forwarded port", tc.pick, tc.on)
		}
		if tomlBool(sectionOf(m, tc.off), "gluetun_port_forward") {
			t.Fatalf("pick %q: %s still follows it too, both would bind the same port", tc.pick, tc.off)
		}
		if got := tomlStr(sectionOf(m, tc.off), "gluetun_url"); got != "" {
			t.Fatalf("pick %q: %s kept a gluetun url %q", tc.pick, tc.off, got)
		}
		if got := gluetunEngineFromTOML(sectionOf(m, "race")); got != tc.on {
			t.Fatalf("pick %q: reading the config back says %q, not %q", tc.pick, got, tc.on)
		}
	}
}

// The warning has to name the engine actually left behind. The first version of
// this page said "hoard takes it, race stays unreachable" as a constant, which
// becomes a lie the moment the operator picks race.
func TestGluetunWarningNamesTheEngineLeftOut(t *testing.T) {
	f := netModeFields{GluetunPort: true, GluetunEngine: "race"}
	got := strings.Join(modeWarnings(netModeGluetun, f, nil), " ")
	if !strings.Contains(got, "The race engine takes its listen port") {
		t.Fatalf("warning does not say race takes the port: %q", got)
	}
	if !strings.Contains(got, "the hoard engine keeps its own") {
		t.Fatalf("warning does not say hoard is the one left out: %q", got)
	}
}

// TestCheckWarningsSeeTheSameFieldsAsThePage — caught on staging, where the
// check confidently reported "the race engine is bound to no interface" about
// two engines both bound to tun1. The check built its own netModeFields literal
// and filled three of them, so any warning reading a fourth read a zero value.
// The fix was one reader for both callers; this is the guard on it.
func TestCheckWarningsSeeTheSameFieldsAsThePage(t *testing.T) {
	race := map[string]interface{}{"listen_port": int64(16171), "bind_interface": "tun1"}
	hoard := map[string]interface{}{"listen_port": int64(16172), "bind_interface": "tun1"}
	f := netFieldsFromTOML(race, hoard)
	if f.RaceBindInterface != "tun1" || f.HoardBindInterface != "tun1" {
		t.Fatalf("interfaces lost on the way out of the config: race=%q hoard=%q", f.RaceBindInterface, f.HoardBindInterface)
	}
	// Both bound: nothing to warn about. A zero-valued struct would produce the
	// "bound to no interface" line twice, which is the bug this guards.
	for _, w := range modeWarnings(netModeGluetun, f, nil) {
		if strings.Contains(w, "bound to no interface") {
			t.Errorf("warned that a bound engine is unbound: %q", w)
		}
	}
}
