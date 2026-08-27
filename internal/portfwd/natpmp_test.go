package portfwd

import (
	"context"
	"encoding/binary"
	"net"
	"net/netip"
	"strings"
	"sync/atomic"
	"testing"
	"time"
)

// fakeGateway is a NAT-PMP gateway on loopback. It answers the way Proton
// does: it ignores the suggested port on a first request and grants 60s.
type fakeGateway struct {
	conn      *net.UDPConn
	port      int
	requests  atomic.Int32
	tcpPort   int
	udpPort   int
	result    uint16
	lifetime  uint32
	silentFor int32
}

func startGateway(t *testing.T, g *fakeGateway) netip.AddrPort {
	t.Helper()
	c, err := net.ListenUDP("udp", &net.UDPAddr{IP: net.IPv4(127, 0, 0, 1)})
	if err != nil {
		t.Fatal(err)
	}
	g.conn = c
	if g.lifetime == 0 {
		g.lifetime = 60
	}
	t.Cleanup(func() { c.Close() })
	go func() {
		buf := make([]byte, 32)
		for {
			n, addr, err := c.ReadFromUDP(buf)
			if err != nil {
				return
			}
			if n < 12 {
				continue
			}
			if g.requests.Add(1) <= g.silentFor {
				continue
			}
			op := buf[1]
			ext := g.udpPort
			if op == opMapTCP {
				ext = g.tcpPort
			}
			resp := make([]byte, 16)
			resp[0] = 0
			resp[1] = op + responseFlag
			binary.BigEndian.PutUint16(resp[2:], g.result)
			binary.BigEndian.PutUint32(resp[4:], 1234)
			copy(resp[8:10], buf[4:6])
			binary.BigEndian.PutUint16(resp[10:], uint16(ext))
			binary.BigEndian.PutUint32(resp[12:], g.lifetime)
			_, _ = c.WriteToUDP(resp, addr)
		}
	}()
	ap := c.LocalAddr().(*net.UDPAddr)
	return netip.AddrPortFrom(netip.AddrFrom4([4]byte{127, 0, 0, 1}), uint16(ap.Port))
}

// clientFor points a NATPMP at the fake gateway. The port is not 5351 in a
// test, so it is injected.
func clientFor(ap netip.AddrPort) NATPMP {
	return NATPMP{Gateway: ap.Addr(), port: int(ap.Port())}
}

func TestAcquireReturnsTheGrantedPort(t *testing.T) {
	g := &fakeGateway{tcpPort: 51234, udpPort: 51234}
	ap := startGateway(t, g)
	port, life, err := clientFor(ap).Acquire(context.Background(), 0, 0)
	if err != nil {
		t.Fatalf("Acquire: %v", err)
	}
	if port != 51234 {
		t.Errorf("port = %d, want 51234", port)
	}
	if life != 60*time.Second {
		t.Errorf("lifetime = %s, want 60s", life)
	}
	// Both protocols, or uTP and the DHT are quietly lost.
	if n := g.requests.Load(); n != 2 {
		t.Errorf("%d requests, want 2 (one TCP, one UDP)", n)
	}
}

// A gateway that forwards two different numbers cannot be announced: one port
// goes in the announce. Reporting the split beats announcing half of it.
func TestAcquireRefusesASplitMapping(t *testing.T) {
	g := &fakeGateway{tcpPort: 51234, udpPort: 51999}
	ap := startGateway(t, g)
	_, _, err := clientFor(ap).Acquire(context.Background(), 0, 0)
	if err == nil {
		t.Fatal("a TCP/UDP split was accepted")
	}
	if !strings.Contains(err.Error(), "51234") || !strings.Contains(err.Error(), "51999") {
		t.Errorf("the error does not name both ports: %v", err)
	}
}

// Result code 2 is the one users actually hit: forwarding is off on the
// account or the server is not a P2P one. It must say that, and it must not
// spend 12 seconds retrying a refusal that cannot change.
func TestRefusalIsReportedImmediatelyAndInWords(t *testing.T) {
	g := &fakeGateway{result: 2}
	ap := startGateway(t, g)
	start := time.Now()
	_, _, err := clientFor(ap).Acquire(context.Background(), 0, 0)
	if err == nil {
		t.Fatal("a refusal was read as success")
	}
	if !strings.Contains(err.Error(), "not enabled") {
		t.Errorf("unhelpful error: %v", err)
	}
	if time.Since(start) > 2*time.Second {
		t.Errorf("a final refusal was retried for %s", time.Since(start))
	}
	if n := g.requests.Load(); n != 1 {
		t.Errorf("%d requests for a refusal, want 1", n)
	}
}

// A tunnel that has just come up drops the first datagrams. Giving up on the
// first timeout would mean no port on every restart.
func TestAcquireRetriesThroughEarlySilence(t *testing.T) {
	g := &fakeGateway{tcpPort: 4321, udpPort: 4321, silentFor: 2}
	ap := startGateway(t, g)
	port, _, err := clientFor(ap).Acquire(context.Background(), 0, 0)
	if err != nil {
		t.Fatalf("gave up on a gateway that was merely slow: %v", err)
	}
	if port != 4321 {
		t.Errorf("port = %d", port)
	}
}

func TestRenewIntervalIsWellInsideTheLease(t *testing.T) {
	if got := RenewInterval(60 * time.Second); got != 30*time.Second {
		t.Errorf("RenewInterval(60s) = %s, want 30s", got)
	}
	// A gateway granting a very short lease must not turn the loop into a
	// packet storm.
	if got := RenewInterval(2 * time.Second); got < 5*time.Second {
		t.Errorf("RenewInterval(2s) = %s, too aggressive", got)
	}
	for _, lease := range []time.Duration{30 * time.Second, 60 * time.Second, 3600 * time.Second} {
		if RenewInterval(lease) >= lease {
			t.Errorf("RenewInterval(%s) does not renew before the lease expires", lease)
		}
	}
}

func TestResultTextNamesTheProviderFix(t *testing.T) {
	if !strings.Contains(resultText(2), "P2P") {
		t.Errorf("code 2 does not tell the operator what to change: %q", resultText(2))
	}
	if resultText(0) != "" {
		t.Errorf("success carries an error text: %q", resultText(0))
	}
}
