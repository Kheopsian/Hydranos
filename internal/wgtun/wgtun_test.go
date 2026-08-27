package wgtun

import (
	"context"
	"errors"
	"fmt"
	"net/netip"
	"strings"
	"syscall"
	"testing"
	"time"
)

const protonConf = `[Interface]
# Key for hydra
# Bouncing = 1
# NAT-PMP (Port Forwarding) = on
PrivateKey = aFakeKeyForTestsOnlyaFakeKeyForTestsOnlyAAA=
Address = 10.2.0.2/32
DNS = 10.2.0.1
MTU = 1380

[Peer]
# FR#173
PublicKey = PeerKeyForTestsOnlyPeerKeyForTestsOnlyBBB=
AllowedIPs = 0.0.0.0/0
Endpoint = 203.0.113.21:51820
`

func mustParse(t *testing.T, text string) *Conf {
	t.Helper()
	c, err := ParseConf(text)
	if err != nil {
		t.Fatalf("ParseConf: %v", err)
	}
	return c
}

func TestParseProtonConf(t *testing.T) {
	c := mustParse(t, protonConf)
	if c.PrivateKey != "aFakeKeyForTestsOnlyaFakeKeyForTestsOnlyAAA=" {
		t.Errorf("private key lost its base64 padding: %q", c.PrivateKey)
	}
	if len(c.Addresses) != 1 || c.Addresses[0].String() != "10.2.0.2/32" {
		t.Errorf("addresses = %v", c.Addresses)
	}
	if c.MTU != 1380 {
		t.Errorf("MTU = %d", c.MTU)
	}
	if len(c.Peers) != 1 || c.Peers[0].Endpoint != "203.0.113.21:51820" {
		t.Fatalf("peers = %+v", c.Peers)
	}
	if got := c.Peers[0].AllowedIPs[0].String(); got != "0.0.0.0/0" {
		t.Errorf("AllowedIPs = %s", got)
	}
}

// The padding case has its own test because Cut on "=" eats it, and a key
// missing its last character is refused by wg with a message about base64 that
// points nowhere near the parser.
func TestParseKeepsBase64Padding(t *testing.T) {
	c := mustParse(t, protonConf)
	for _, k := range []string{c.PrivateKey, c.Peers[0].PublicKey} {
		if !strings.HasSuffix(k, "=") {
			t.Errorf("key %q lost its padding", k)
		}
	}
}

func TestParseRejectsWhatWouldFailLater(t *testing.T) {
	cases := map[string]string{
		"no private key": "[Interface]\nAddress = 10.2.0.2/32\n[Peer]\nPublicKey = k=\nEndpoint = a:1\n",
		"no peer":        "[Interface]\nPrivateKey = k=\nAddress = 10.2.0.2/32\n",
		"no address":     "[Interface]\nPrivateKey = k=\n[Peer]\nPublicKey = k=\nEndpoint = a:1\n",
		"peer no key":    "[Interface]\nPrivateKey = k=\nAddress = 10.2.0.2/32\n[Peer]\nEndpoint = a:1\n",
		"no endpoint":    "[Interface]\nPrivateKey = k=\nAddress = 10.2.0.2/32\n[Peer]\nPublicKey = k=\n",
	}
	for name, text := range cases {
		if _, err := ParseConf(text); err == nil {
			t.Errorf("%s: accepted a config that cannot work", name)
		}
	}
}

// wg refuses a whole file over one wg-quick-only key, so the rendered text
// must carry none of them. Asserted by absence, which is the only way this
// particular failure shows up before runtime.
func TestSetconfTextDropsWgQuickOnlyKeys(t *testing.T) {
	text := mustParse(t, protonConf).SetconfText()
	for _, forbidden := range []string{"Address", "DNS", "MTU", "Table", "PostUp"} {
		if strings.Contains(text, forbidden) {
			t.Errorf("setconf text still carries %q, wg will refuse the file:\n%s", forbidden, text)
		}
	}
	for _, needed := range []string{"PrivateKey", "PublicKey", "Endpoint", "AllowedIPs"} {
		if !strings.Contains(text, needed) {
			t.Errorf("setconf text is missing %q", needed)
		}
	}
}

func TestRedactedHidesTheKey(t *testing.T) {
	c := mustParse(t, protonConf)
	r := c.Redacted()
	if strings.Contains(r.PrivateKey, "FakeKey") {
		t.Error("the private key survived redaction")
	}
	if c.PrivateKey == r.PrivateKey {
		t.Error("Redacted mutated the original instead of copying")
	}
}

func specFor(t *testing.T) Spec {
	return Spec{Device: "hy-race", Table: TableFor(0), RulePriority: RulePriorityFor(0), Conf: mustParse(t, protonConf)}
}

// The guard that matters. Everything else in this package is convenience; this
// is the one whose failure means a host silently moved its whole egress into a
// VPN tunnel.
func TestUpPlanNeverTouchesTheMainRoutingTable(t *testing.T) {
	steps, err := UpPlan(specFor(t))
	if err != nil {
		t.Fatal(err)
	}
	sawRoute := false
	for _, s := range steps {
		line := s.String()
		isRoute := strings.Contains(line, " route add")
		if !isRoute {
			continue
		}
		sawRoute = true
		if !strings.Contains(line, fmt.Sprintf("table %d", TableFor(0))) {
			t.Errorf("route installed outside our table: %q", line)
		}
		for _, reserved := range []string{"table main", "table 254", "table local", "table 255", "table default", "table 253"} {
			if strings.Contains(line, reserved) {
				t.Errorf("route installed in a reserved table: %q", line)
			}
		}
	}
	if !sawRoute {
		t.Fatal("the plan installs no route at all: a device pinned socket would get ENETUNREACH")
	}
}

// A rule at or above 32766 is consulted after the main table, so the tunnel
// route is never reached. The tunnel looks perfect and carries nothing.
func TestUpPlanRuleSitsBelowTheMainTableRule(t *testing.T) {
	steps, _ := UpPlan(specFor(t))
	found := false
	for _, s := range steps {
		line := s.String()
		if !strings.Contains(line, "rule add") {
			continue
		}
		found = true
		if !strings.Contains(line, "oif hy-race") {
			t.Errorf("rule does not select by device: %q", line)
		}
		if strings.Contains(line, "from ") {
			t.Errorf("rule selects by source address: %q -- two Proton tunnels share 10.2.0.2, so this matches the wrong one", line)
		}
	}
	if !found {
		t.Fatal("no ip rule in the plan: the tunnel table would never be consulted")
	}
	if RulePriorityFor(0) >= 32766 {
		t.Fatalf("rule priority %d is not below the kernel main-table rule", RulePriorityFor(0))
	}
}

func TestValidateRefusesReservedTablesAndBadNames(t *testing.T) {
	base := specFor(t)
	for _, tbl := range []int{0, 253, 254, 255} {
		s := base
		s.Table = tbl
		if err := s.Validate(); err == nil {
			t.Errorf("table %d accepted", tbl)
		}
	}
	s := base
	s.Device = "hy-an-absurdly-long-name"
	if err := s.Validate(); err == nil {
		t.Error("a device name over IFNAMSIZ was accepted")
	}
	s = base
	s.RulePriority = 40000
	if err := s.Validate(); err == nil {
		t.Error("a rule priority above the main-table rule was accepted")
	}
}

func TestDeviceNameFor(t *testing.T) {
	cases := map[string]string{
		"race":                          "hy-race",
		"hoard":                         "hy-hoard",
		"Race_Extra 2":                  "hy-race-extra-2",
		"an-engine-with-a-very-long-id": "hy-an-engine-wi",
	}
	for in, want := range cases {
		if got := DeviceNameFor(in); got != want {
			t.Errorf("DeviceNameFor(%q) = %q, want %q", in, got, want)
		}
		if len(DeviceNameFor(in)) > 15 {
			t.Errorf("DeviceNameFor(%q) is over IFNAMSIZ", in)
		}
	}
}

func TestGatewayIsTheFirstAddressOfTheTunnel(t *testing.T) {
	c := mustParse(t, protonConf)
	gw, err := Gateway(c.Addresses)
	if err != nil {
		t.Fatal(err)
	}
	if gw.String() != "10.2.0.1" {
		t.Errorf("gateway = %s, want 10.2.0.1", gw)
	}
}

func TestProviderPortForwardKinds(t *testing.T) {
	cases := map[string]PortForwardKind{
		"proton":  PortForwardNATPMP,
		"mullvad": PortForwardNone,
		"airvpn":  PortForwardManual,
		"pia":     PortForwardManual,
	}
	for id, want := range cases {
		p, ok := LookupProvider(id)
		if !ok {
			t.Errorf("%s unknown", id)
		}
		if p.PortForward != want {
			t.Errorf("%s: port forward %q, want %q", id, p.PortForward, want)
		}
	}
	if p, ok := LookupProvider("no-such-vpn"); ok || p.PortForward != PortForwardNone {
		t.Errorf("an unknown provider should degrade to generic/none, got %+v ok=%v", p, ok)
	}
}

// fakeRunner records the command line of everything the manager runs.
type fakeRunner struct {
	seen []string
	fail map[string]string
	out  map[string]string
}

func (f *fakeRunner) Run(_ context.Context, args ...string) (string, error) {
	line := strings.Join(args, " ")
	f.seen = append(f.seen, line)
	for pat, msg := range f.fail {
		if strings.Contains(line, pat) {
			return "", fmt.Errorf("%s", msg)
		}
	}
	for pat, o := range f.out {
		if strings.Contains(line, pat) {
			return o, nil
		}
	}
	return "", nil
}

func TestUpRunsThePlanAndSubstitutesTheConfPath(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	fr := &fakeRunner{}
	m := NewManager(t.TempDir())
	m.SetRunner(fr)
	if err := m.Up(context.Background(), "race", "proton", specFor(t)); err != nil {
		t.Fatalf("Up: %v", err)
	}
	joined := strings.Join(fr.seen, "\n")
	if strings.Contains(joined, ConfPathPlaceholder) {
		t.Error("the conf placeholder was passed to wg verbatim")
	}
	if !strings.Contains(joined, "wg setconf hy-race ") {
		t.Errorf("no setconf ran:\n%s", joined)
	}
	if !strings.Contains(joined, "ip link add dev hy-race type wireguard") {
		t.Errorf("the device was never created:\n%s", joined)
	}
	if strings.Contains(joined, "wg-quick") {
		t.Error("wg-quick was invoked, which is the one thing this package must never do")
	}
}

// A failing step must stop the plan, not carry on into a half-built tunnel
// that reports itself up.
func TestUpStopsAtTheFirstRealFailure(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	fr := &fakeRunner{fail: map[string]string{"wg setconf": "wrong key"}}
	m := NewManager(t.TempDir())
	m.SetRunner(fr)
	if err := m.Up(context.Background(), "race", "proton", specFor(t)); err == nil {
		t.Fatal("Up reported success while setconf failed")
	}
	for _, line := range fr.seen {
		if strings.Contains(line, "route add") {
			t.Errorf("kept going after the failure and installed a route: %q", line)
		}
	}
}

// The leftovers from a previous run are removed before creating anything, and
// those deletions are the ONLY steps allowed to fail silently.
func TestUpToleratesMissingLeftoversOnly(t *testing.T) {
	steps, _ := UpPlan(specFor(t))
	for _, s := range steps {
		if !s.IgnoreError {
			continue
		}
		if !strings.Contains(s.String(), " del") {
			t.Errorf("a non-deletion step is allowed to fail silently: %q", s)
		}
	}
}

// Only the v6 half may degrade. If a v4 step ever became soft, a tunnel with
// no usable route would report itself up and the engine would sit on it.
func TestOnlyTheIPv6HalfIsAllowedToDegrade(t *testing.T) {
	dual := mustParse(t, protonConf)
	dual.Addresses = append(dual.Addresses, mustPrefix(t, "2a07:b944::2:2/128"))
	steps, err := UpPlan(Spec{Device: "hy-race", Table: TableFor(0), RulePriority: RulePriorityFor(0), Conf: dual})
	if err != nil {
		t.Fatal(err)
	}
	softV6 := 0
	for _, s := range steps {
		if s.SoftFail && s.Family != "6" {
			t.Errorf("a non-IPv6 step may fail without failing the tunnel: %q", s)
		}
		if s.SoftFail && s.Family == "6" {
			softV6++
		}
	}
	if softV6 == 0 {
		t.Fatal("no v6 step is degradable: a host with IPv6 off loses the whole tunnel")
	}
}

// The real-hardware failure this came from: Unraid disables IPv6 on every new
// interface, so `ip -6 address add` fails on a device we just created.
func TestDualStackPlanEnablesIPv6OnItsOwnDevice(t *testing.T) {
	dual := mustParse(t, protonConf)
	dual.Addresses = append(dual.Addresses, mustPrefix(t, "2a07:b944::2:2/128"))
	steps, _ := UpPlan(Spec{Device: "hy-race", Table: TableFor(0), RulePriority: RulePriorityFor(0), Conf: dual})
	var sysctlAt, addrAt = -1, -1
	for i, s := range steps {
		line := s.String()
		if strings.Contains(line, "disable_ipv6") {
			if !strings.Contains(line, "/proc/sys/net/ipv6/conf/hy-race/") {
				t.Errorf("the sysctl is not scoped to our own device: %q", line)
			}
			sysctlAt = i
		}
		if strings.Contains(line, "ip -6 address add") {
			addrAt = i
		}
	}
	if sysctlAt < 0 {
		t.Fatal("nothing enables IPv6 on the device, so the v6 address is refused on any host that defaults it off")
	}
	if addrAt < sysctlAt {
		t.Error("the v6 address is added before IPv6 is enabled on the device")
	}
}

func TestIPv4OnlyPlanSaysNothingAboutIPv6(t *testing.T) {
	steps, _ := UpPlan(specFor(t))
	for _, s := range steps {
		if strings.Contains(s.String(), "disable_ipv6") || strings.Contains(s.String(), "ip -6 address") {
			t.Errorf("a v4-only config produced an IPv6 step: %q", s)
		}
	}
}

func mustPrefix(t *testing.T, s string) netip.Prefix {
	t.Helper()
	p, err := netip.ParsePrefix(s)
	if err != nil {
		t.Fatal(err)
	}
	return p
}

// The manager must keep going after the v6 half fails, and must still install
// the v4 route -- the exact shape of the first live run on Orion.
func TestUpSurvivesAHostWithIPv6Disabled(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	dual := mustParse(t, protonConf)
	dual.Addresses = append(dual.Addresses, mustPrefix(t, "2a07:b944::2:2/128"))
	fr := &fakeRunner{fail: map[string]string{
		"ip -6 address add": "Error: ipv6: IPv6 is disabled on this device.",
	}}
	m := NewManager(t.TempDir())
	m.SetRunner(fr)
	spec := Spec{Device: "hy-race", Table: TableFor(0), RulePriority: RulePriorityFor(0), Conf: dual}
	if err := m.Up(context.Background(), "race", "proton", spec); err != nil {
		t.Fatalf("the whole tunnel was lost over its IPv6 half: %v", err)
	}
	joined := strings.Join(fr.seen, "\n")
	if !strings.Contains(joined, "ip -4 route add default dev hy-race table") {
		t.Errorf("the v4 route was never installed:\n%s", joined)
	}
	if strings.Contains(joined, "ip -6 route add") {
		t.Error("the v6 route was attempted after the v6 address was refused")
	}
	st := m.States(context.Background())
	if len(st) != 1 || !strings.Contains(st[0].LastError, "IPv6") {
		t.Errorf("the degradation is not reported anywhere: %+v", st)
	}
}

func TestWaitHandshakeReturnsWhenThePeerAnswers(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	m := NewManager(t.TempDir())
	m.SetRunner(&fakeRunner{out: map[string]string{"latest-handshakes": "peerkey\t1756300000\n"}})
	if err := m.WaitHandshake(context.Background(), "hy-race", time.Second); err != nil {
		t.Fatalf("WaitHandshake: %v", err)
	}
}

func TestWaitHandshakeGivesUpLoudlyWhenItNeverComes(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	m := NewManager(t.TempDir())
	m.SetRunner(&fakeRunner{out: map[string]string{"latest-handshakes": "peerkey\t0\n"}})
	err := m.WaitHandshake(context.Background(), "hy-race", 900*time.Millisecond)
	if err == nil {
		t.Fatal("a tunnel that never handshakes was reported as up")
	}
	if !strings.Contains(err.Error(), "handshake") {
		t.Errorf("the error does not say what failed: %v", err)
	}
}

func TestDownRemovesTheRuleAndTheDevice(t *testing.T) {
	if !Supported() {
		t.Skip("linux only")
	}
	fr := &fakeRunner{}
	m := NewManager(t.TempDir())
	m.SetRunner(fr)
	_ = m.Up(context.Background(), "race", "proton", specFor(t))
	fr.seen = nil
	if err := m.Down(context.Background(), "hy-race"); err != nil {
		t.Fatal(err)
	}
	joined := strings.Join(fr.seen, "\n")
	// The rule outlives the device it names, so forgetting it leaves a stale
	// rule that silently captures the next tunnel on the same priority.
	if !strings.Contains(joined, "rule del priority") {
		t.Errorf("the routing rule was left behind:\n%s", joined)
	}
	if !strings.Contains(joined, "ip link del dev hy-race") {
		t.Errorf("the device was left behind:\n%s", joined)
	}
}

// The message an operator gets when IPv6 cannot be enabled has to name the
// flag to add. Without it the visible symptom -- a tunnel with no v6 -- points
// at the kernel, the provider or the config, none of which are the cause.
func TestReadOnlyProcExplainsWhichFlagToAdd(t *testing.T) {
	msg := explainWriteFailure("/proc/sys/net/ipv6/conf/hy-race/disable_ipv6", syscall.EROFS)
	for _, want := range []string{"read-only", "--sysctl", "disable_ipv6=0"} {
		if !strings.Contains(msg, want) {
			t.Errorf("the message does not mention %q: %s", want, msg)
		}
	}
	if !strings.Contains(msg, "IPv4") {
		t.Errorf("the message does not say the tunnel still works over IPv4: %s", msg)
	}
	// An unrelated failure must not be dressed up as this one.
	other := explainWriteFailure("/proc/sys/net/ipv6/conf/hy-race/disable_ipv6", errors.New("no such file"))
	if strings.Contains(other, "--sysctl") {
		t.Errorf("an unrelated error was reported as a read-only mount: %s", other)
	}
}
