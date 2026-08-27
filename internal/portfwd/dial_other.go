//go:build !linux

package portfwd

import (
	"context"
	"errors"
	"net"
)

// dial is unimplemented off Linux. NAT-PMP itself is portable; pinning the
// request to one tunnel device is not, and an unpinned request on a
// multi-tunnel host asks the wrong gateway and gets a port forwarded on
// somebody else's tunnel. Refusing is the honest answer -- the Windows agent
// takes its port from the operator, as it takes its interface.
func (n NATPMP) dial(context.Context) (net.Conn, error) {
	return nil, errors.New("automatic port forwarding is only supported on Linux; set the port by hand")
}
