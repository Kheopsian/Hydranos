package wgtun

import (
	"fmt"
	"net/netip"
	"sort"
	"strings"
)

// PortForwardKind is how -- or whether -- an incoming port can be obtained on
// a given provider's tunnel.
//
// This is the field that makes the provider name worth asking for at all. A
// WireGuard config says nothing about port forwarding: two identical-looking
// .conf files, one from Proton and one from Mullvad, differ only in that one
// will hand out a port on request and the other has not offered forwarding
// since 2023. Guessing wrong is not a visible error -- the tunnel comes up,
// the engine seeds, and only the inbound half is dead, which looks exactly
// like a slow swarm.
type PortForwardKind string

const (
	// PortForwardNATPMP asks the tunnel gateway over NAT-PMP. The mapping is
	// short-lived and must be renewed, which is why it is a running loop and
	// not a startup step.
	PortForwardNATPMP PortForwardKind = "natpmp"
	// PortForwardManual means the provider assigns a port out of band (a web
	// portal), so the user types it and nothing renews it.
	PortForwardManual PortForwardKind = "manual"
	// PortForwardNone means the provider does not forward ports at all. Said
	// out loud rather than left blank: an engine on such a tunnel takes no
	// incoming connections, ever, and the operator should know that before
	// wondering why its ratio is flat.
	PortForwardNone PortForwardKind = "none"
	// PortForwardGluetun keeps the existing path: a gluetun control server
	// already negotiated the port, we just read it.
	PortForwardGluetun PortForwardKind = "gluetun"
)

// Provider describes what Hydra knows about one VPN provider.
type Provider struct {
	ID          string
	Label       string
	PortForward PortForwardKind
	// Note is shown in the UI beside the choice. Empty for the boring ones.
	Note string
}

var providers = map[string]Provider{
	"proton": {
		ID: "proton", Label: "Proton VPN", PortForward: PortForwardNATPMP,
		Note: "The port is obtained by NAT-PMP and renewed continuously. Use a server marked P2P.",
	},
	"airvpn": {
		ID: "airvpn", Label: "AirVPN", PortForward: PortForwardManual,
		Note: "AirVPN assigns the port in the client area. Create it there, then type it here.",
	},
	"mullvad": {
		ID: "mullvad", Label: "Mullvad", PortForward: PortForwardNone,
		Note: "Mullvad removed port forwarding in 2023. This engine will take no incoming peer connections.",
	},
	"pia": {
		ID: "pia", Label: "Private Internet Access", PortForward: PortForwardManual,
		Note: "PIA forwards ports through its own API, which needs the account credentials as well as the config. Not automated yet: set the port by hand, or run PIA behind gluetun.",
	},
	"windscribe": {
		ID: "windscribe", Label: "Windscribe", PortForward: PortForwardManual,
		Note: "Windscribe assigns an ephemeral or static port on its web panel.",
	},
	"natpmp": {
		ID: "natpmp", Label: "Other (NAT-PMP capable)", PortForward: PortForwardNATPMP,
		Note: "For any provider whose gateway answers NAT-PMP, the way Proton does.",
	},
	"generic": {
		ID: "generic", Label: "Other / none", PortForward: PortForwardNone,
		Note: "The tunnel is brought up, no port is requested. Set a port by hand if the provider forwards one.",
	},
}

// LookupProvider resolves a provider id. Unknown ids fall back to "generic"
// with ok=false rather than to an error: a config naming a provider we have
// not heard of should still bring its tunnel up. Only the port half degrades.
func LookupProvider(id string) (Provider, bool) {
	p, ok := providers[strings.ToLower(strings.TrimSpace(id))]
	if !ok {
		return providers["generic"], false
	}
	return p, true
}

// Providers lists what the UI offers, in a stable order.
func Providers() []Provider {
	out := make([]Provider, 0, len(providers))
	for _, p := range providers {
		out = append(out, p)
	}
	sort.Slice(out, func(i, j int) bool { return out[i].Label < out[j].Label })
	return out
}

// Gateway guesses the NAT-PMP gateway of a tunnel from its own address.
//
// WireGuard has no notion of a gateway -- there is no DHCP, no router
// advertisement, nothing in the .conf that names one. Every NAT-PMP-capable
// provider we know of puts it at the first address of the tunnel's subnet, and
// Proton hands out a /32, which carries no subnet to speak of. So the rule is:
// take the interface address and clear the last octet to 1 (10.2.0.2 ->
// 10.2.0.1), which is what every wg NAT-PMP client in the wild does.
//
// It is a guess, and it is labelled as one: when the mapping request times out
// the caller reports the address it tried, so a provider that puts its gateway
// elsewhere produces a message an operator can act on instead of a silent
// absence of port.
func Gateway(addrs []netip.Prefix) (netip.Addr, error) {
	for _, p := range addrs {
		a := p.Addr()
		if !a.Is4() {
			continue
		}
		b := a.As4()
		b[3] = 1
		return netip.AddrFrom4(b), nil
	}
	return netip.Addr{}, fmt.Errorf("the tunnel has no IPv4 address, so no NAT-PMP gateway can be derived")
}
