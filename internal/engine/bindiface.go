package engine

import (
	"fmt"
	"log/slog"
	"net"
	"strings"
)

// pinDialerToInterface makes every socket a dialer opens leave by ONE named
// interface, and returns the error that must fail the dial when it cannot.
//
// It pins by NAME (SO_BINDTODEVICE), not by source address, because a source
// address does not choose a route. ProtonVPN hands every tunnel the same
// 10.2.0.2, so a source-IP bind cannot tell wg0 from wg1 and the kernel routes
// both by destination, out whichever tunnel holds the default route.
//
// Measured on two Proton tunnels (FR#173 and FR#373) in a dedicated netns:
//
//	source-IP bind    wg0 -> 203.0.113.21   wg1 -> 203.0.113.21   (same, wrong)
//	SO_BINDTODEVICE   wg0 -> 203.0.113.21   wg1 -> 203.0.113.22    (right)
//
// So every engine of a multi-tunnel setup was announcing through one tunnel,
// with no error and no log, while the header showed one exit address for all of
// them. That is exactly the leak bind_interface exists to close, and it was not
// closed: the pin was real but powerless.
//
// The source address is still pinned when the interface carries an IPv4, both
// because it is the correct source for that device and because the announce
// path reads a pinned binding as IPv4-only.
func pinDialerToInterface(d *net.Dialer, iface string, fwmark int) (bool, error) {
	iface = strings.TrimSpace(iface)
	applyEgressControl(d, iface, fwmark)
	if iface == "" {
		return false, nil
	}
	if _, err := net.InterfaceByName(iface); err != nil {
		return false, fmt.Errorf("bind_interface %q does not resolve: %w", iface, err)
	}
	ip, err := resolveInterfaceIP(iface)
	if err != nil {
		if !bindToDeviceSupported {
			// Nothing left to pin with: refuse rather than dial from the
			// default route, which is what the tunnel exists to avoid.
			return false, fmt.Errorf("bind_interface %q has no IPv4 to bind to: %w", iface, err)
		}
		// The device pin alone still steers the egress, which is the part that
		// matters. Worth one line: an interface with no IPv4 is unusual enough
		// that an operator seeing v4 announces fail wants to know why.
		slog.Warn("bind_interface has no IPv4 address; the egress is pinned to the device, but v4 announces have no source",
			"interface", iface, "error", err)
		return true, nil
	}
	d.LocalAddr = &net.TCPAddr{IP: net.ParseIP(ip)}
	return true, nil
}
