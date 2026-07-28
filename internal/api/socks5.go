package api

import (
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"strconv"
	"strings"
	"time"

	"github.com/Kheopsian/hydra/internal/config"
)

// SOCKS5Dialer opens TCP connections to arbitrary targets through a SOCKS5
// proxy with optional username/password auth (RFC 1928 + RFC 1929).
type SOCKS5Dialer struct {
	ProxyAddr string
	User      string
	Pass      string
	Timeout   time.Duration
}

// Dial opens a TCP connection to target (host:port) via the SOCKS5 proxy.
func (d *SOCKS5Dialer) Dial(ctx context.Context, target string) (net.Conn, error) {
	timeout := d.Timeout
	if timeout == 0 {
		timeout = 10 * time.Second
	}
	dctx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	var nd net.Dialer
	conn, err := nd.DialContext(dctx, "tcp", d.ProxyAddr)
	if err != nil {
		return nil, fmt.Errorf("socks5 dial proxy %s: %w", d.ProxyAddr, err)
	}
	if dl, ok := dctx.Deadline(); ok {
		_ = conn.SetDeadline(dl)
	}
	if err := d.handshake(conn, target); err != nil {
		_ = conn.Close()
		return nil, err
	}
	_ = conn.SetDeadline(time.Time{})
	return conn, nil
}

// DialContext matches http.Transport.DialContext signature.
func (d *SOCKS5Dialer) DialContext(ctx context.Context, network, addr string) (net.Conn, error) {
	switch network {
	case "tcp", "tcp4", "tcp6":
	default:
		return nil, fmt.Errorf("socks5: unsupported network %q", network)
	}
	return d.Dial(ctx, addr)
}

func (d *SOCKS5Dialer) handshake(conn net.Conn, target string) error {
	useAuth := d.User != "" || d.Pass != ""
	method := byte(0x00)
	if useAuth {
		method = 0x02
	}
	if _, err := conn.Write([]byte{0x05, 0x01, method}); err != nil {
		return fmt.Errorf("socks5 greet: %w", err)
	}
	reply := make([]byte, 2)
	if _, err := io.ReadFull(conn, reply); err != nil {
		return fmt.Errorf("socks5 greet reply: %w", err)
	}
	if reply[0] != 0x05 {
		return fmt.Errorf("socks5 bad version %x", reply[0])
	}
	if reply[1] == 0xff {
		return errors.New("socks5 no acceptable auth method")
	}
	if reply[1] != method {
		return fmt.Errorf("socks5 method mismatch got=%x want=%x", reply[1], method)
	}

	if useAuth {
		if len(d.User) > 255 || len(d.Pass) > 255 {
			return errors.New("socks5 creds too long")
		}
		buf := make([]byte, 0, 3+len(d.User)+len(d.Pass))
		buf = append(buf, 0x01, byte(len(d.User)))
		buf = append(buf, d.User...)
		buf = append(buf, byte(len(d.Pass)))
		buf = append(buf, d.Pass...)
		if _, err := conn.Write(buf); err != nil {
			return fmt.Errorf("socks5 auth send: %w", err)
		}
		authReply := make([]byte, 2)
		if _, err := io.ReadFull(conn, authReply); err != nil {
			return fmt.Errorf("socks5 auth reply: %w", err)
		}
		if authReply[0] != 0x01 || authReply[1] != 0x00 {
			return fmt.Errorf("socks5 auth rejected status=%x", authReply[1])
		}
	}

	host, portStr, err := net.SplitHostPort(target)
	if err != nil {
		return fmt.Errorf("socks5 target: %w", err)
	}
	portN, err := strconv.Atoi(portStr)
	if err != nil {
		return fmt.Errorf("socks5 port: %w", err)
	}
	req := []byte{0x05, 0x01, 0x00}
	if ip := net.ParseIP(host); ip != nil {
		if ip4 := ip.To4(); ip4 != nil {
			req = append(req, 0x01)
			req = append(req, ip4...)
		} else {
			req = append(req, 0x04)
			req = append(req, ip.To16()...)
		}
	} else {
		if len(host) > 255 {
			return errors.New("socks5 hostname too long")
		}
		req = append(req, 0x03, byte(len(host)))
		req = append(req, host...)
	}
	portBuf := make([]byte, 2)
	binary.BigEndian.PutUint16(portBuf, uint16(portN))
	req = append(req, portBuf...)
	if _, err := conn.Write(req); err != nil {
		return fmt.Errorf("socks5 connect send: %w", err)
	}

	head := make([]byte, 4)
	if _, err := io.ReadFull(conn, head); err != nil {
		return fmt.Errorf("socks5 connect reply: %w", err)
	}
	if head[0] != 0x05 {
		return fmt.Errorf("socks5 reply bad version %x", head[0])
	}
	if head[1] != 0x00 {
		return fmt.Errorf("socks5 connect failed rep=%x", head[1])
	}
	var skip int
	switch head[3] {
	case 0x01:
		skip = 4
	case 0x04:
		skip = 16
	case 0x03:
		lenByte := make([]byte, 1)
		if _, err := io.ReadFull(conn, lenByte); err != nil {
			return fmt.Errorf("socks5 reply addr len: %w", err)
		}
		skip = int(lenByte[0])
	default:
		return fmt.Errorf("socks5 reply bad atyp %x", head[3])
	}
	tail := make([]byte, skip+2)
	if _, err := io.ReadFull(conn, tail); err != nil {
		return fmt.Errorf("socks5 reply tail: %w", err)
	}
	return nil
}

// --- Module-level proxy used by getPublicIP and vpn_speedtest ---------------

var socks5Proxy atomicSocks5

type atomicSocks5 struct{ p *SOCKS5Dialer }

// SetSocks5Proxy installs the shared SOCKS5 dialer. Passing nil disables
// proxying (fetches/speedtests then use the default egress path — which on
// this host leaks the LAN IP; only use for rollback).
func SetSocks5Proxy(d *SOCKS5Dialer) { socks5Proxy.p = d }

func getSocks5Proxy() *SOCKS5Dialer { return socks5Proxy.p }

// NewSOCKS5DialerFromConfig builds a dialer from TOML settings. Returns nil
// when the config is empty (proxy disabled).
func NewSOCKS5DialerFromConfig(cfg config.ProxyConfig) *SOCKS5Dialer {
	if cfg.Socks5Host == "" || cfg.Socks5Port == 0 {
		return nil
	}
	host := cfg.Socks5Host
	if strings.Contains(host, ":") {
		host = "[" + host + "]"
	}
	return &SOCKS5Dialer{
		ProxyAddr: fmt.Sprintf("%s:%d", host, cfg.Socks5Port),
		User:      cfg.Socks5User,
		Pass:      cfg.Socks5Pass,
	}
}
