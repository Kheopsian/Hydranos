//go:build linux

package portfwd

import (
	"context"
	"net"
	"net/netip"
	"syscall"

	"golang.org/x/sys/unix"
)

// dial opens the UDP socket, pinned to the tunnel device.
func (n NATPMP) dial(ctx context.Context) (net.Conn, error) {
	d := &net.Dialer{}
	if n.Device != "" {
		dev := n.Device
		d.Control = func(_, _ string, c syscall.RawConn) error {
			var serr error
			if err := c.Control(func(fd uintptr) {
				serr = unix.SetsockoptString(int(fd), unix.SOL_SOCKET, unix.SO_BINDTODEVICE, dev)
			}); err != nil {
				return err
			}
			return serr
		}
	}
	return d.DialContext(ctx, "udp", netip.AddrPortFrom(n.Gateway, uint16(n.gatewayPort())).String())
}
