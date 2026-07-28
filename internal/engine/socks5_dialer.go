package engine

import (
	"bufio"
	"context"
	"encoding/binary"
	"fmt"
	"io"
	"net"
)

// dialSOCKS5h opens a TCP connection to socksAddr, authenticates with
// user/pass if user is non-empty (RFC 1929), sends a CONNECT request for
// (host, port) using ATYP=domainname (RFC 1928), and returns the data conn
// ready for the caller's higher-level protocol (TLS, HTTP, etc.).
//
// "h" in SOCKS5h means we send the literal hostname to the proxy and let
// it resolve — useful when our local stack has no usable DNS path to the
// target (or when we want to hide the lookup from the local network).
//
// The returned conn wraps a bufio.Reader if the proxy buffered data after
// the BND reply; reads go through it transparently.
func dialSOCKS5h(ctx context.Context, dialer *net.Dialer, socksAddr, user, pass, host string, port uint16) (net.Conn, error) {
	conn, err := dialer.DialContext(ctx, "tcp", socksAddr)
	if err != nil {
		return nil, fmt.Errorf("socks5: dial proxy %s: %w", socksAddr, err)
	}
	success := false
	defer func() {
		if !success {
			conn.Close()
		}
	}()

	// Greeting — RFC 1928 §3.
	var greet []byte
	if user != "" {
		greet = []byte{5, 1, 2} // METHOD 2 = USERNAME/PASSWORD
	} else {
		greet = []byte{5, 1, 0} // METHOD 0 = NO AUTH
	}
	if _, err := conn.Write(greet); err != nil {
		return nil, fmt.Errorf("socks5: greet write: %w", err)
	}
	resp := make([]byte, 2)
	if _, err := io.ReadFull(conn, resp); err != nil {
		return nil, fmt.Errorf("socks5: greet read: %w", err)
	}
	if resp[0] != 5 {
		return nil, fmt.Errorf("socks5: server VER=%d (want 5)", resp[0])
	}

	// Auth subprotocol — RFC 1929.
	if resp[1] == 2 {
		if len(user) > 255 || len(pass) > 255 {
			return nil, fmt.Errorf("socks5: cred over 255 bytes")
		}
		buf := make([]byte, 0, 3+len(user)+len(pass))
		buf = append(buf, 1, byte(len(user)))
		buf = append(buf, []byte(user)...)
		buf = append(buf, byte(len(pass)))
		buf = append(buf, []byte(pass)...)
		if _, err := conn.Write(buf); err != nil {
			return nil, fmt.Errorf("socks5: auth write: %w", err)
		}
		ar := make([]byte, 2)
		if _, err := io.ReadFull(conn, ar); err != nil {
			return nil, fmt.Errorf("socks5: auth read: %w", err)
		}
		if ar[0] != 1 || ar[1] != 0 {
			return nil, fmt.Errorf("socks5: auth rejected status=%d", ar[1])
		}
	} else if resp[1] != 0 {
		return nil, fmt.Errorf("socks5: unsupported method %d", resp[1])
	}

	// CONNECT request — VER=5, CMD=1 (CONNECT), RSV=0, ATYP=3 (DOMAINNAME).
	if len(host) > 255 {
		return nil, fmt.Errorf("socks5: host over 255 bytes")
	}
	req := make([]byte, 0, 7+len(host))
	req = append(req, 5, 1, 0, 3, byte(len(host)))
	req = append(req, []byte(host)...)
	req = binary.BigEndian.AppendUint16(req, port)
	if _, err := conn.Write(req); err != nil {
		return nil, fmt.Errorf("socks5: connect write: %w", err)
	}

	// CONNECT reply — VER, REP, RSV, ATYP, BND.ADDR, BND.PORT.
	br := bufio.NewReader(conn)
	head := make([]byte, 4)
	if _, err := io.ReadFull(br, head); err != nil {
		return nil, fmt.Errorf("socks5: connect resp head: %w", err)
	}
	if head[0] != 5 {
		return nil, fmt.Errorf("socks5: reply VER=%d", head[0])
	}
	if head[1] != 0 {
		return nil, fmt.Errorf("socks5: REP=%d (rejected)", head[1])
	}
	var skip int64
	switch head[3] {
	case 1: // IPv4
		skip = 4 + 2
	case 3: // domainname
		lb, err := br.ReadByte()
		if err != nil {
			return nil, fmt.Errorf("socks5: bnd atyp3 len: %w", err)
		}
		skip = int64(lb) + 2
	case 4: // IPv6
		skip = 16 + 2
	default:
		return nil, fmt.Errorf("socks5: bnd unknown ATYP=%d", head[3])
	}
	if _, err := io.CopyN(io.Discard, br, skip); err != nil {
		return nil, fmt.Errorf("socks5: skip bnd: %w", err)
	}

	success = true
	return &bufferedSocksConn{Conn: conn, br: br}, nil
}

// bufferedSocksConn wraps the proxy conn with a bufio.Reader so reads see
// any bytes the proxy may have buffered ahead of the BND.PORT field.
type bufferedSocksConn struct {
	net.Conn
	br *bufio.Reader
}

func (b *bufferedSocksConn) Read(p []byte) (int, error) {
	return b.br.Read(p)
}
