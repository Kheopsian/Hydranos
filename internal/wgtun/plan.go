package wgtun

import (
	"fmt"
	"net/netip"
	"strconv"
	"strings"
)

// ConfPathPlaceholder is substituted by the executor with the path of a
// temporary, 0600 file holding the `wg setconf` text. The plan cannot know it
// -- the file does not exist yet when the plan is built -- and the plan is
// what the tests read, so the placeholder is part of the contract rather than
// a detail of the runner.
const ConfPathPlaceholder = "@CONF@"

// Step is one command in a tunnel plan.
//
// The plan is built separately from being run, and that split is the point:
// what this package does to a machine's routing is exactly the list of steps
// below, so a test can read the list and assert what is NOT in it -- no write
// to the main routing table, no default route outside our own table -- without
// needing root, a network namespace, or a real tunnel.
type Step struct {
	Args []string
	// IgnoreError marks a step whose failure is not a failure of the plan:
	// deleting something that was already gone, mostly. Kept explicit so that
	// every OTHER step is fatal by default, rather than the reverse.
	IgnoreError bool
	// Family is "4", "6" or "" (both). It exists so that the failure of one
	// family can skip the REST of that family instead of killing the tunnel:
	// a host with IPv6 switched off still wants the v4 half of a dual-stack
	// provider config, and losing the whole tunnel over an address the
	// operator never asked for would be the wrong trade.
	Family string
	// SoftFail marks a step whose failure degrades the tunnel rather than
	// failing it. Only ever set on the v6 half, and always reported: a
	// degraded tunnel that says nothing is how "why is IPv6 not working"
	// becomes a two-hour investigation.
	SoftFail bool
	Desc     string
}

func (s Step) String() string { return strings.Join(s.Args, " ") }

// Spec is one tunnel to bring up.
type Spec struct {
	// Device is the interface name. Created by us, owned by us, and deleted by
	// us: it must not be an interface the operator manages, because Up starts
	// by deleting whatever carries this name.
	Device string
	// Table is the routing table every route for this tunnel goes into. It is
	// never the main table, and Validate refuses the reserved ones.
	Table int
	// RulePriority orders the `ip rule` entry. Distinct per tunnel so two
	// tunnels never fight over one slot.
	RulePriority int
	MTU          int
	Conf         *Conf
}

// Reserved routing tables. Writing a default route into any of these is the
// failure mode this whole package exists to avoid, so it is refused at the
// type level rather than reviewed for.
const (
	tableUnspec  = 0
	tableDefault = 253
	tableMain    = 254
	tableLocal   = 255
)

// TableBase is where per-tunnel tables start. Far from the ranges iproute2
// names in rt_tables and far from anything a user is likely to have written by
// hand, while staying well inside the 32-bit id space.
const TableBase = 7770

// RulePriorityBase is where our `ip rule` entries sit, and it has to be BELOW
// the kernel's own main-table rule at 32766. Above it, the main table would be
// consulted first and would answer for 0.0.0.0/0 on any host that has a
// default route -- which is every host -- so the tunnel would come up, show a
// handshake, and carry nothing.
const RulePriorityBase = 7770

// TableFor and RulePriorityFor derive a tunnel's slot from its index, so two
// engines configured in the same file never collide.
func TableFor(index int) int        { return TableBase + index }
func RulePriorityFor(index int) int { return RulePriorityBase + index }

// DefaultMTU is what wg-quick would compute; providers that need another value
// state it in the .conf and that wins.
const DefaultMTU = 1420

// Validate refuses a spec that would touch something it must not.
func (s Spec) Validate() error {
	if strings.TrimSpace(s.Device) == "" {
		return fmt.Errorf("no device name")
	}
	if len(s.Device) > 15 {
		// IFNAMSIZ. A name over the limit fails inside `ip link add` with a
		// message that does not mention the length.
		return fmt.Errorf("device name %q is longer than the 15 characters Linux allows", s.Device)
	}
	switch s.Table {
	case tableUnspec, tableDefault, tableMain, tableLocal:
		return fmt.Errorf("routing table %d is reserved: a tunnel route must never land in the main tables", s.Table)
	}
	if s.Table < 0 || s.Table > 0xFFFFFFF {
		return fmt.Errorf("routing table %d is out of range", s.Table)
	}
	if s.RulePriority <= 0 || s.RulePriority >= 32766 {
		// At or above 32766 the main table answers first and the tunnel is
		// never consulted -- a tunnel that comes up, shows a handshake, and
		// carries nothing.
		return fmt.Errorf("rule priority %d must sit below the kernel's main-table rule (32766)", s.RulePriority)
	}
	if s.Conf == nil {
		return fmt.Errorf("no parsed configuration")
	}
	if len(s.Conf.Addresses) == 0 {
		return fmt.Errorf("the configuration carries no Address")
	}
	return nil
}

func (s Spec) mtu() int {
	if s.Conf != nil && s.Conf.MTU > 0 {
		return s.Conf.MTU
	}
	if s.MTU > 0 {
		return s.MTU
	}
	return DefaultMTU
}

func (s Spec) families() (v4, v6 bool) {
	for _, a := range s.Conf.Addresses {
		if a.Addr().Is4() {
			v4 = true
		} else {
			v6 = true
		}
	}
	return
}

// UpPlan is every command needed to take the tunnel from "not there" to
// "usable by a socket bound to it".
//
// Read as a whole it is the answer to "what does Hydra do to my machine": it
// creates ONE interface, gives it the address from the config, and installs
// its routes in a table nothing consults unless a socket was explicitly bound
// to that interface. Nothing else on the host changes.
func UpPlan(s Spec) ([]Step, error) {
	if err := s.Validate(); err != nil {
		return nil, err
	}
	dev := s.Device
	tbl := strconv.Itoa(s.Table)
	prio := strconv.Itoa(s.RulePriority)
	v4, v6 := s.families()

	steps := []Step{
		// Start from a clean slate. A previous run that died without cleaning
		// up leaves the device behind, and `ip link add` on an existing name
		// fails; reusing it blind would keep the OLD peer and key while the
		// UI shows the new ones.
		{Args: []string{"ip", "rule", "del", "priority", prio}, IgnoreError: true,
			Desc: "drop a leftover rule from a previous run"},
		{Args: []string{"ip", "-6", "rule", "del", "priority", prio}, IgnoreError: true,
			Desc: "same, IPv6 side"},
		{Args: []string{"ip", "link", "del", "dev", dev}, IgnoreError: true,
			Desc: "drop a leftover device from a previous run"},
		{Args: []string{"ip", "link", "add", "dev", dev, "type", "wireguard"},
			Desc: "create the tunnel device"},
		{Args: []string{"wg", "setconf", dev, ConfPathPlaceholder},
			Desc: "load the keys and peers"},
	}
	if v6 {
		// Many hosts disable IPv6 on every new interface by default (Unraid
		// does), and a provider config that carries a v6 address then fails at
		// `ip -6 address add` with "IPv6 is disabled on this device" -- a
		// message that reads like a kernel problem and is really a per-device
		// sysctl. Turn it on for OUR device only: it is one we created, and
		// nothing else on the host is touched.
		steps = append(steps, Step{
			Args:   []string{"sysctl", "-w", "net.ipv6.conf." + sysctlName(dev) + ".disable_ipv6=0"},
			Family: "6", SoftFail: true,
			Desc: "allow IPv6 on the tunnel device",
		})
	}
	for _, a := range s.Conf.Addresses {
		fam, family := "-4", "4"
		if !a.Addr().Is4() {
			fam, family = "-6", "6"
		}
		steps = append(steps, Step{
			Args:     []string{"ip", fam, "address", "add", a.String(), "dev", dev},
			Family:   family,
			SoftFail: family == "6",
			Desc:     "give the tunnel its source address",
		})
	}
	steps = append(steps,
		Step{Args: []string{"ip", "link", "set", "mtu", strconv.Itoa(s.mtu()), "up", "dev", dev},
			Desc: "bring the tunnel up"},
	)
	// The routes. Every one of them carries `table <ours>`, and that word is
	// the only thing standing between this feature and a host that has
	// silently moved its whole egress into a VPN.
	if v4 {
		steps = append(steps,
			Step{Args: []string{"ip", "-4", "route", "add", "default", "dev", dev, "table", tbl},
				Desc: "default route, in our table only"},
			Step{Args: []string{"ip", "-4", "rule", "add", "oif", dev, "lookup", tbl, "priority", prio},
				Desc: "consult that table for sockets bound to this device"},
		)
	}
	if v6 {
		steps = append(steps,
			Step{Args: []string{"ip", "-6", "route", "add", "default", "dev", dev, "table", tbl},
				Family: "6", SoftFail: true,
				Desc: "default route, in our table only"},
			Step{Args: []string{"ip", "-6", "rule", "add", "oif", dev, "lookup", tbl, "priority", prio},
				Family: "6", SoftFail: true,
				Desc: "consult that table for sockets bound to this device"},
		)
	}
	// Deliberately NOT installed: a `from <tunnel address> lookup <table>`
	// rule. It looks like the obvious companion to the oif rule and it is a
	// trap -- Proton hands every one of its tunnels the same 10.2.0.2, so a
	// from-rule on a two-tunnel host matches both and the kernel picks by
	// priority. That is precisely the bug that made two engines announce
	// through one tunnel with every indicator green. The device is the only
	// unambiguous selector, so it is the only one used.
	return steps, nil
}

// DownPlan removes what UpPlan installed.
//
// Deleting the device takes its routes with it, so the table needs no separate
// cleanup; the rule does, because a rule survives the interface it names and a
// stale one would silently capture the next tunnel that lands on the same
// priority.
func DownPlan(s Spec) []Step {
	prio := strconv.Itoa(s.RulePriority)
	return []Step{
		{Args: []string{"ip", "rule", "del", "priority", prio}, IgnoreError: true,
			Desc: "remove the IPv4 rule"},
		{Args: []string{"ip", "-6", "rule", "del", "priority", prio}, IgnoreError: true,
			Desc: "remove the IPv6 rule"},
		{Args: []string{"ip", "link", "del", "dev", s.Device}, IgnoreError: true,
			Desc: "remove the tunnel device"},
	}
}

// DeviceNameFor builds the interface name for an engine.
//
// Prefixed so that an operator reading `ip link` can tell at a glance which
// interfaces Hydra owns -- and so that Up, which deletes the device before
// creating it, can never be pointed at wg0 by a careless config.
func DeviceNameFor(engineID string) string {
	clean := strings.Map(func(r rune) rune {
		switch {
		case r >= 'a' && r <= 'z', r >= '0' && r <= '9', r == '-':
			return r
		case r >= 'A' && r <= 'Z':
			return r + 32
		default:
			return '-'
		}
	}, strings.TrimSpace(engineID))
	name := "hy-" + clean
	if len(name) > 15 {
		name = name[:15]
	}
	return strings.TrimRight(name, "-")
}

// GatewayFor is Gateway over a spec, for callers that already hold one.
func (s Spec) GatewayFor() (netip.Addr, error) { return Gateway(s.Conf.Addresses) }

// sysctlName escapes an interface name for a sysctl key. Dots in an interface
// name (VLAN devices carry them) would otherwise split the key.
func sysctlName(dev string) string { return strings.ReplaceAll(dev, ".", "/") }
