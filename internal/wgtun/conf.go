// Package wgtun brings WireGuard tunnels up for the engines that ask for one,
// and it does that WITHOUT wg-quick.
//
// The distinction is the whole reason this package exists. A provider .conf
// carries AllowedIPs = 0.0.0.0/0, and wg-quick reads that as "make this the
// default route of the machine". On a host that also serves a library, runs a
// hypervisor or simply has to stay reachable, that is a footgun with no undo:
// the box changes its own egress the moment the tunnel comes up.
//
// So the .conf is PARSED, never EXECUTED. The address, the peers and the keys
// are taken from it; the routing decision is ours. Every route this package
// installs lands in a table of its own, reachable only by a rule matching
// packets that were explicitly bound to the tunnel device. The main routing
// table is never written to. A process that does not ask for the tunnel cannot
// accidentally end up inside it, and the host's default route is untouched --
// verifiable with `ip route show` before and after.
//
// That pairs with how engines already pin themselves: SO_BINDTODEVICE on every
// socket (see internal/engine/bindiface.go and typhon-engine/src/netpin.rs).
// Binding a socket to a device is what makes the rule match, and the rule is
// what gives that device a usable route. Neither half works alone -- a socket
// pinned to a device with no route gets ENETUNREACH, and a route nobody is
// bound to is never consulted.
package wgtun

import (
	"bufio"
	"fmt"
	"net/netip"
	"strconv"
	"strings"
)

// Peer is one [Peer] section of a wg config.
type Peer struct {
	PublicKey    string
	PresharedKey string
	Endpoint     string
	AllowedIPs   []netip.Prefix
	Keepalive    int
}

// Conf is a parsed wg-quick style configuration.
//
// The wg-quick-only keys (Address, DNS, MTU, Table, PreUp...) are kept
// separate from the ones `wg setconf` understands, because handing wg a file
// containing Address = makes it refuse the whole file. Splitting them here is
// what lets the same .conf a provider hands out be used verbatim.
type Conf struct {
	PrivateKey string
	Addresses  []netip.Prefix
	ListenPort int
	FwMark     int
	MTU        int
	DNS        []string
	Peers      []Peer
	// Table is the raw value of the wg-quick Table= key, kept only so we can
	// say out loud that we ignore it. We never install a default route.
	Table string
}

// ParseConf reads a wg-quick style configuration.
//
// It is deliberately strict about the two things that would fail LATER and
// silently -- a missing private key, and a peer without a public key -- and
// deliberately lax about keys it does not use: provider configs carry all
// sorts of extras, and refusing a file over a comment nobody reads would just
// push the user back to wg-quick.
func ParseConf(text string) (*Conf, error) {
	c := &Conf{}
	var section string
	var cur *Peer
	sc := bufio.NewScanner(strings.NewReader(text))
	sc.Buffer(make([]byte, 0, 64*1024), 1024*1024)
	line := 0
	flush := func() {
		if cur != nil {
			c.Peers = append(c.Peers, *cur)
			cur = nil
		}
	}
	for sc.Scan() {
		line++
		s := strings.TrimSpace(sc.Text())
		if i := strings.IndexAny(s, "#;"); i >= 0 {
			s = strings.TrimSpace(s[:i])
		}
		if s == "" {
			continue
		}
		if strings.HasPrefix(s, "[") && strings.HasSuffix(s, "]") {
			name := strings.ToLower(strings.TrimSpace(s[1 : len(s)-1]))
			switch name {
			case "interface":
				flush()
				section = "interface"
			case "peer":
				flush()
				section = "peer"
				cur = &Peer{}
			default:
				return nil, fmt.Errorf("line %d: unknown section [%s]", line, name)
			}
			continue
		}
		k, v, ok := strings.Cut(s, "=")
		if !ok {
			return nil, fmt.Errorf("line %d: not a key = value line: %q", line, s)
		}
		key := strings.ToLower(strings.TrimSpace(k))
		val := strings.TrimSpace(v)
		// A base64 key ends in '=' padding, which Cut just ate. Give it back.
		if key == "privatekey" || key == "publickey" || key == "presharedkey" {
			val = strings.TrimSpace(s[len(k)+1:])
		}
		switch section {
		case "interface":
			if err := c.setInterfaceKey(key, val, line); err != nil {
				return nil, err
			}
		case "peer":
			if err := setPeerKey(cur, key, val, line); err != nil {
				return nil, err
			}
		default:
			return nil, fmt.Errorf("line %d: %q appears before any section", line, key)
		}
	}
	if err := sc.Err(); err != nil {
		return nil, err
	}
	flush()
	if c.PrivateKey == "" {
		return nil, fmt.Errorf("no PrivateKey in [Interface]")
	}
	if len(c.Peers) == 0 {
		return nil, fmt.Errorf("no [Peer] section: nothing to connect to")
	}
	for i, p := range c.Peers {
		if p.PublicKey == "" {
			return nil, fmt.Errorf("peer %d has no PublicKey", i+1)
		}
		if p.Endpoint == "" && len(c.Peers) == 1 {
			return nil, fmt.Errorf("the only peer has no Endpoint: nothing to dial")
		}
	}
	if len(c.Addresses) == 0 {
		return nil, fmt.Errorf("no Address in [Interface]: the tunnel would have no source address")
	}
	return c, nil
}

func (c *Conf) setInterfaceKey(key, val string, line int) error {
	switch key {
	case "privatekey":
		c.PrivateKey = val
	case "address":
		for _, part := range splitList(val) {
			p, err := parseCIDR(part)
			if err != nil {
				return fmt.Errorf("line %d: Address %q: %w", line, part, err)
			}
			c.Addresses = append(c.Addresses, p)
		}
	case "listenport":
		n, err := strconv.Atoi(val)
		if err != nil {
			return fmt.Errorf("line %d: ListenPort %q is not a number", line, val)
		}
		c.ListenPort = n
	case "fwmark":
		n, err := strconv.ParseInt(strings.TrimPrefix(val, "0x"), 16, 64)
		if err != nil {
			if d, derr := strconv.Atoi(val); derr == nil {
				c.FwMark = d
				return nil
			}
			return fmt.Errorf("line %d: FwMark %q is not a number", line, val)
		}
		c.FwMark = int(n)
	case "mtu":
		n, err := strconv.Atoi(val)
		if err != nil {
			return fmt.Errorf("line %d: MTU %q is not a number", line, val)
		}
		c.MTU = n
	case "dns":
		c.DNS = append(c.DNS, splitList(val)...)
	case "table":
		c.Table = val
	case "preup", "postup", "predown", "postdown", "saveconfig":
		// Ignored on purpose, and this is the sharpest edge in the file: those
		// hooks are arbitrary shell, and a config downloaded from a provider
		// portal is not a thing to hand to a shell running as root. wg-quick
		// runs them. We do not, and we say so rather than pretending the file
		// was fully honoured.
		return nil
	default:
		return nil
	}
	return nil
}

func setPeerKey(p *Peer, key, val string, line int) error {
	if p == nil {
		return fmt.Errorf("line %d: %q outside a [Peer] section", line, key)
	}
	switch key {
	case "publickey":
		p.PublicKey = val
	case "presharedkey":
		p.PresharedKey = val
	case "endpoint":
		p.Endpoint = val
	case "allowedips":
		for _, part := range splitList(val) {
			pre, err := parseCIDR(part)
			if err != nil {
				return fmt.Errorf("line %d: AllowedIPs %q: %w", line, part, err)
			}
			p.AllowedIPs = append(p.AllowedIPs, pre)
		}
	case "persistentkeepalive":
		n, err := strconv.Atoi(val)
		if err != nil {
			return fmt.Errorf("line %d: PersistentKeepalive %q is not a number", line, val)
		}
		p.Keepalive = n
	}
	return nil
}

func splitList(v string) []string {
	var out []string
	for _, part := range strings.Split(v, ",") {
		if s := strings.TrimSpace(part); s != "" {
			out = append(out, s)
		}
	}
	return out
}

// parseCIDR accepts both "10.2.0.2/32" and the bare "10.2.0.2" some configs
// carry, treating the bare form as a host address.
func parseCIDR(s string) (netip.Prefix, error) {
	if strings.Contains(s, "/") {
		return netip.ParsePrefix(s)
	}
	a, err := netip.ParseAddr(s)
	if err != nil {
		return netip.Prefix{}, err
	}
	return netip.PrefixFrom(a, a.BitLen()), nil
}

// SetconfText renders the subset `wg setconf` understands.
//
// Everything wg-quick-only (Address, DNS, MTU, Table, hooks) is dropped: wg
// rejects the entire file over one such key, which is why a provider .conf
// cannot be passed to `wg setconf` as-is.
//
// AllowedIPs is carried through verbatim. It is a CRYPTO decision -- which
// source addresses this peer may claim -- and narrowing it here would silently
// drop traffic the tunnel is supposed to carry. The ROUTING consequence of
// 0.0.0.0/0, which is the dangerous half, is neutralised elsewhere: see
// tunnelRoutes, which never touches the main table.
func (c *Conf) SetconfText() string {
	var b strings.Builder
	b.WriteString("[Interface]\n")
	fmt.Fprintf(&b, "PrivateKey = %s\n", c.PrivateKey)
	if c.ListenPort > 0 {
		fmt.Fprintf(&b, "ListenPort = %d\n", c.ListenPort)
	}
	if c.FwMark > 0 {
		fmt.Fprintf(&b, "FwMark = %d\n", c.FwMark)
	}
	for _, p := range c.Peers {
		b.WriteString("\n[Peer]\n")
		fmt.Fprintf(&b, "PublicKey = %s\n", p.PublicKey)
		if p.PresharedKey != "" {
			fmt.Fprintf(&b, "PresharedKey = %s\n", p.PresharedKey)
		}
		if p.Endpoint != "" {
			fmt.Fprintf(&b, "Endpoint = %s\n", p.Endpoint)
		}
		if len(p.AllowedIPs) > 0 {
			strs := make([]string, 0, len(p.AllowedIPs))
			for _, a := range p.AllowedIPs {
				strs = append(strs, a.String())
			}
			fmt.Fprintf(&b, "AllowedIPs = %s\n", strings.Join(strs, ", "))
		}
		if p.Keepalive > 0 {
			fmt.Fprintf(&b, "PersistentKeepalive = %d\n", p.Keepalive)
		}
	}
	return b.String()
}

// Redacted returns the conf with the private key replaced, for logs and for
// anything that leaves this process. The key is the one thing in the file that
// must never reach an API response: /api/settings already serves secrets in
// clear, so a tunnel key that got into the config tree would be served with
// them.
func (c *Conf) Redacted() *Conf {
	cp := *c
	cp.PrivateKey = "(redacted)"
	for i := range cp.Peers {
		if cp.Peers[i].PresharedKey != "" {
			cp.Peers[i].PresharedKey = "(redacted)"
		}
	}
	return &cp
}
