//go:build linux

package wgtun

import (
	"context"
	"os"
	"os/exec"
	"strings"
	"testing"
)

// The root test. Everything else in this package asserts what the plan SAYS;
// this one runs it against a real kernel and checks what the machine looks
// like afterwards.
//
// It is the guard that matters, and until now it existed only as something I
// did by hand once on the production host: bring two tunnels up, diff
// `ip route` and `ip rule` before and after, tear them down, diff again. That
// is not a guard, that is a memory. Here it runs on every pull request.
//
// Gated on HYDRA_WG_ROOT_TEST because it creates interfaces and routing rules:
// nobody running `go test ./...` on their laptop should discover that
// afterwards.
func TestRootTunnelLeavesTheHostRoutingAlone(t *testing.T) {
	if os.Getenv("HYDRA_WG_ROOT_TEST") != "1" {
		t.Skip("set HYDRA_WG_ROOT_TEST=1 to run the tests that create real interfaces")
	}
	if os.Geteuid() != 0 {
		t.Fatal("this test needs root: it creates a WireGuard device")
	}
	for _, bin := range []string{"ip", "wg"} {
		if _, err := exec.LookPath(bin); err != nil {
			t.Fatalf("%s is missing: install iproute2 and wireguard-tools", bin)
		}
	}

	show := func(args ...string) string {
		out, err := exec.Command("ip", args...).CombinedOutput()
		if err != nil {
			t.Fatalf("ip %s: %v", strings.Join(args, " "), err)
		}
		return string(out)
	}
	routesBefore, rulesBefore := show("route", "show"), show("rule", "show")

	// A key pair and a peer that answers nothing: the handshake is not what is
	// under test here, the effect on the host's routing is.
	key, err := exec.Command("wg", "genkey").Output()
	if err != nil {
		t.Fatalf("wg genkey: %v", err)
	}
	priv := strings.TrimSpace(string(key))
	pubOut := exec.Command("wg", "pubkey")
	pubOut.Stdin = strings.NewReader(priv)
	pub, err := pubOut.Output()
	if err != nil {
		t.Fatalf("wg pubkey: %v", err)
	}
	conf := "[Interface]\nPrivateKey = " + priv + "\nAddress = 10.253.0.2/32\n\n" +
		"[Peer]\nPublicKey = " + strings.TrimSpace(string(pub)) + "\n" +
		"AllowedIPs = 0.0.0.0/0\nEndpoint = 192.0.2.1:51820\n"
	parsed, err := ParseConf(conf)
	if err != nil {
		t.Fatalf("ParseConf: %v", err)
	}

	m := NewManager(t.TempDir())
	spec := Spec{Device: "hy-roottest", Table: TableFor(99), RulePriority: RulePriorityFor(99), Conf: parsed}
	ctx := context.Background()
	t.Cleanup(func() { _ = m.Down(ctx, spec.Device) })
	if err := m.Up(ctx, "roottest", "generic", spec); err != nil {
		t.Fatalf("Up: %v", err)
	}

	// What SHOULD have happened.
	if !strings.Contains(show("link", "show", "dev", spec.Device), spec.Device) {
		t.Errorf("the device was not created")
	}
	ourTable := show("route", "show", "table", "7869")
	if !strings.Contains(ourTable, "default") || !strings.Contains(ourTable, spec.Device) {
		t.Errorf("our table has no default route through the device:\n%s", ourTable)
	}
	if !strings.Contains(show("rule", "show"), "oif "+spec.Device) {
		t.Errorf("no rule selects the tunnel by device")
	}

	// What must NOT have happened. This is the whole test.
	if got := show("route", "show"); got != routesBefore {
		t.Errorf("the host's main routing table changed:\nbefore:\n%s\nafter:\n%s", routesBefore, got)
	}

	if err := m.Down(ctx, spec.Device); err != nil {
		t.Fatalf("Down: %v", err)
	}
	if got := show("route", "show"); got != routesBefore {
		t.Errorf("the main table is still altered after teardown:\n%s", got)
	}
	// A rule outlives the interface it names, so a forgotten one silently
	// captures the next tunnel that lands on the same priority.
	if got := show("rule", "show"); got != rulesBefore {
		t.Errorf("a routing rule was left behind:\nbefore:\n%s\nafter:\n%s", rulesBefore, got)
	}
	// The exit code, not the text: `ip link show dev X` on a missing device
	// prints `Device "X" does not exist`, which contains the name. Matching on
	// the name would report a device that is gone as still present -- a test
	// that fails forever and says the opposite of what happened.
	if err := exec.Command("ip", "link", "show", "dev", spec.Device).Run(); err == nil {
		t.Errorf("the device survived teardown")
	}
}
