package engine

import (
	"context"
	"fmt"
	"io"
	"net"
	"net/http"
	"strings"
	"time"
)

// DefaultEchoURL is the service asked "what address do you see me from". Plain
// HTTP on purpose for the peer-path probe below, which speaks a raw socket and
// has no TLS stack of its own; the announce probe uses the same URL over the
// announcer's own client, TLS included.
const DefaultEchoURL = "https://api.ipify.org/"

// AnnounceEgressIP reports the address a tracker sees when this binding
// announces, by asking an echo service THROUGH the binding's own announce
// client.
//
// Building the real announcer is the entire point. Any other client would
// measure what the config claims instead of what the announce path does, and
// that gap is exactly the defect this probe exists to expose: peers relayed,
// announces direct, every other indicator green.
func AnnounceEgressIP(ctx context.Context, b Binding, echoURL string) (string, error) {
	if strings.TrimSpace(echoURL) == "" {
		echoURL = DefaultEchoURL
	}
	ta := newTrackerAnnouncerForBinding(b)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, echoURL, nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("User-Agent", ta.userAgent)
	resp, err := ta.httpClient.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("echo service answered HTTP %d", resp.StatusCode)
	}
	return readEchoIP(resp.Body)
}

// PeerEgressIP reports the address a peer sees when we dial it: through the
// SOCKS5 proxy when one is configured, otherwise a direct dial pinned to the
// same interface the engine binds its peer sockets to.
//
// ⚠ This RECONSTRUCTS the peer path rather than borrowing the engine's own
// socket — peer dials live in the Rust engine and cannot be driven from here.
// Same proxy, same source address, so it answers the operator's question; but a
// defect inside the engine's own dial would not show up in it. Said plainly in
// the UI rather than hidden behind a green tick.
func PeerEgressIP(ctx context.Context, socksHost string, socksPort int, socksUser, socksPass, bindInterface, echoURL string) (string, error) {
	if strings.TrimSpace(echoURL) == "" {
		echoURL = DefaultEchoURL
	}
	dialer := &net.Dialer{Timeout: 8 * time.Second}
	if strings.TrimSpace(socksHost) == "" && strings.TrimSpace(bindInterface) != "" {
		ip, err := resolveInterfaceIP(bindInterface)
		if err != nil {
			return "", fmt.Errorf("bind_interface %q: %w", bindInterface, err)
		}
		dialer.LocalAddr = &net.TCPAddr{IP: net.ParseIP(ip)}
	}
	transport := &http.Transport{DisableKeepAlives: true}
	if h := strings.TrimSpace(socksHost); h != "" {
		proxyAddr := net.JoinHostPort(h, fmt.Sprint(socksPort))
		transport.DialContext = func(ctx context.Context, network, addr string) (net.Conn, error) {
			host, portStr, err := net.SplitHostPort(addr)
			if err != nil {
				return nil, err
			}
			var port uint16
			if _, err := fmt.Sscanf(portStr, "%d", &port); err != nil {
				return nil, err
			}
			return dialSOCKS5h(ctx, dialer, proxyAddr, socksUser, socksPass, host, port)
		}
	} else {
		transport.DialContext = dialer.DialContext
	}
	client := &http.Client{Timeout: 12 * time.Second, Transport: transport}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, echoURL, nil)
	if err != nil {
		return "", err
	}
	resp, err := client.Do(req)
	if err != nil {
		return "", err
	}
	defer resp.Body.Close()
	if resp.StatusCode != http.StatusOK {
		return "", fmt.Errorf("echo service answered HTTP %d", resp.StatusCode)
	}
	return readEchoIP(resp.Body)
}

// readEchoIP takes the first line of an echo response and insists it parses as
// an address. A captive portal or an error page would otherwise be displayed as
// though it were our egress address, which is worse than reporting a failure.
func readEchoIP(r io.Reader) (string, error) {
	b, err := io.ReadAll(io.LimitReader(r, 128))
	if err != nil {
		return "", err
	}
	s := strings.TrimSpace(string(b))
	if i := strings.IndexAny(s, "\r\n"); i >= 0 {
		s = s[:i]
	}
	if net.ParseIP(s) == nil {
		return "", fmt.Errorf("echo service did not return an IP address (%q)", truncate(s, 40))
	}
	return s, nil
}

func truncate(s string, n int) string {
	if len(s) <= n {
		return s
	}
	return s[:n] + "…"
}
