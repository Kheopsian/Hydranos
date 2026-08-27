// Package portfwd obtains the incoming port an engine listens on when that
// port is not ours to choose.
//
// Behind a VPN the listening port is assigned by the provider, per lease, and
// it rotates. A fixed port behind such a tunnel is wrong the moment the lease
// turns over, and wrong in the worst way: the node keeps announcing, keeps
// looking healthy, and simply stops taking incoming peers. Nothing logs it,
// because from the engine's side nothing happened.
//
// So the port is asked for, bound, announced, and then FOLLOWED.
package portfwd

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"net/netip"
	"time"
)

// NAT-PMP, RFC 6886. Proton VPN speaks it on the tunnel gateway, and so do
// several other providers and most consumer routers.
const (
	natpmpPort     = 5351
	natpmpVersion  = 0
	opMapUDP       = 1
	opMapTCP       = 2
	responseFlag   = 128
	natpmpTimeout  = 3 * time.Second
	natpmpAttempts = 4
)

// Lifetime is what we ask for. Proton grants 60 seconds whatever is requested,
// so the renewal loop runs on the GRANTED value, not this one.
const Lifetime = 60 * time.Second

// Mapping is one granted port mapping.
type Mapping struct {
	Internal int
	External int
	Lifetime time.Duration
	Epoch    uint32
}

// NATPMP asks a tunnel's gateway for a forwarded port.
type NATPMP struct {
	Gateway netip.Addr
	// port overrides the well-known 5351. Only tests set it: a real gateway
	// listens where the RFC says it does.
	port int
	// Device pins the request to the tunnel it concerns. Without it the
	// request leaves by the host's default route and either reaches nothing or
	// -- worse, on a multi-tunnel host -- reaches the WRONG gateway and returns
	// a port forwarded on a tunnel this engine does not use. Every Proton
	// tunnel answers on 10.2.0.1, so the address alone cannot disambiguate.
	// This is the same lesson as the source-IP bind that pinned nothing.
	Device string
}

var errShortReply = errors.New("the gateway sent a reply too short to be NAT-PMP")

// resultText turns the protocol's result code into something an operator can
// act on. Left as numbers, "result 2" sends people to an RFC.
func resultText(code uint16) string {
	switch code {
	case 0:
		return ""
	case 1:
		return "the gateway speaks a newer version of NAT-PMP than we do"
	case 2:
		return "the gateway refused: port forwarding is not enabled on this connection (on Proton, enable NAT-PMP in the config download and pick a P2P server)"
	case 3:
		return "the gateway has no network"
	case 4:
		return "the gateway is out of resources"
	case 5:
		return "the gateway does not support this operation"
	}
	return fmt.Sprintf("the gateway returned result code %d", code)
}

// Map requests one mapping and returns what was actually granted.
//
// suggested may be 0, which asks the gateway to choose. On renewal the current
// external port is passed back so the lease keeps the same number: a renewal
// that silently moves the port is indistinguishable, from the engine's side,
// from a renewal that worked.
func (n NATPMP) Map(ctx context.Context, tcp bool, internal, suggested int, lifetime time.Duration) (Mapping, error) {
	if !n.Gateway.IsValid() {
		return Mapping{}, errors.New("no gateway to ask")
	}
	op := byte(opMapUDP)
	if tcp {
		op = opMapTCP
	}
	req := make([]byte, 12)
	req[0] = natpmpVersion
	req[1] = op
	binary.BigEndian.PutUint16(req[4:], uint16(internal))
	binary.BigEndian.PutUint16(req[6:], uint16(suggested))
	binary.BigEndian.PutUint32(req[8:], uint32(lifetime.Seconds()))

	conn, err := n.dial(ctx)
	if err != nil {
		return Mapping{}, err
	}
	defer conn.Close()

	var lastErr error
	// RFC 6886 wants exponential backoff; four tries over ~12s is enough to
	// ride out a tunnel that has just come up and is not passing traffic yet.
	for attempt := 0; attempt < natpmpAttempts; attempt++ {
		if err := conn.SetDeadline(time.Now().Add(natpmpTimeout)); err != nil {
			return Mapping{}, err
		}
		if _, err := conn.Write(req); err != nil {
			lastErr = err
			continue
		}
		buf := make([]byte, 16)
		nread, err := conn.Read(buf)
		if err != nil {
			lastErr = fmt.Errorf("no answer from %s:%d through %s: %w", n.Gateway, natpmpPort, n.Device, err)
			continue
		}
		if nread < 16 {
			lastErr = errShortReply
			continue
		}
		if buf[1] != op+responseFlag {
			// An answer to a different question. Reading it as ours is how a
			// TCP mapping gets reported as the UDP one.
			lastErr = fmt.Errorf("the gateway answered opcode %d, not %d", buf[1], op+responseFlag)
			continue
		}
		if code := binary.BigEndian.Uint16(buf[2:]); code != 0 {
			// A refusal is final: retrying cannot change the answer, and
			// retrying hides it behind a timeout instead.
			return Mapping{}, errors.New(resultText(code))
		}
		return Mapping{
			Epoch:    binary.BigEndian.Uint32(buf[4:]),
			Internal: int(binary.BigEndian.Uint16(buf[8:])),
			External: int(binary.BigEndian.Uint16(buf[10:])),
			Lifetime: time.Duration(binary.BigEndian.Uint32(buf[12:])) * time.Second,
		}, nil
	}
	if lastErr == nil {
		lastErr = errors.New("no answer")
	}
	return Mapping{}, lastErr
}

// Acquire asks for BOTH protocols and returns the port.
//
// Both, because BitTorrent needs both: TCP for peers and UDP for uTP and the
// DHT. A setup that forwards only TCP works well enough to look correct and
// quietly loses every uTP peer, which on some swarms is most of them.
//
// The two mappings must land on the SAME number -- that number is what gets
// announced -- so the TCP grant is passed back as the suggestion for UDP, and
// a gateway that refuses to match is reported rather than papered over.
func (n NATPMP) Acquire(ctx context.Context, internal, suggested int) (int, time.Duration, error) {
	tcpMap, err := n.Map(ctx, true, internal, suggested, Lifetime)
	if err != nil {
		return 0, 0, fmt.Errorf("TCP: %w", err)
	}
	udpMap, err := n.Map(ctx, false, internal, tcpMap.External, Lifetime)
	if err != nil {
		return 0, 0, fmt.Errorf("UDP (TCP got %d): %w", tcpMap.External, err)
	}
	if udpMap.External != tcpMap.External {
		return 0, 0, fmt.Errorf("the gateway forwarded TCP %d but UDP %d: one announced port cannot cover both",
			tcpMap.External, udpMap.External)
	}
	life := tcpMap.Lifetime
	if udpMap.Lifetime < life {
		life = udpMap.Lifetime
	}
	if life <= 0 {
		life = Lifetime
	}
	return tcpMap.External, life, nil
}

// RenewInterval is when to re-ask, given what the gateway granted.
//
// Half the lease, floored at five seconds. Proton grants 60s, so this asks
// every 30 -- twice as often as strictly needed, which is the point: one lost
// datagram must not cost the mapping, because losing it means the announced
// port stops answering and nothing says so until the swarm has moved on.
func RenewInterval(granted time.Duration) time.Duration {
	half := granted / 2
	if half < 5*time.Second {
		return 5 * time.Second
	}
	return half
}

// gatewayPort is the well-known NAT-PMP port unless a test moved it.
func (n NATPMP) gatewayPort() int {
	if n.port > 0 {
		return n.port
	}
	return natpmpPort
}
