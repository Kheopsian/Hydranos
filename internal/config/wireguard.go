package config

import (
	"fmt"
	"path/filepath"
	"strings"
)

// WireGuardConfig makes an engine bring up its OWN WireGuard tunnel, from a
// provider configuration file, instead of expecting one the operator set up
// outside Hydra.
//
// Why it lives on the session rather than somewhere global: a tunnel belongs to
// exactly one engine. That is the whole shape of multi-VPN here -- one agent,
// one engine, one tunnel, one exit address, one forwarded port. A tunnel shared
// by two engines would put both behind one address and one port, which is the
// arrangement this series has spent its time undoing.
//
// The private key is NOT in this struct, and that is deliberate. It stays in
// the .conf file, referenced by name. Everything in a SessionConfig travels:
// into default.toml, into an apply_config frame pushed to a remote agent, into
// /api/settings -- which serves secrets in clear to anyone the API lets in. A
// key kept out of the struct cannot leak through any of them.
type WireGuardConfig struct {
	// Enabled turns the tunnel on. Off, every other field is inert, and the
	// engine's egress is whatever bind_interface says -- the pre-existing
	// behaviour for an operator who manages tunnels themselves.
	Enabled bool `toml:"enabled" json:"enabled"`
	// Provider selects what is known about port forwarding. See internal/wgtun.
	// It has no effect on the tunnel itself: a config is a config.
	Provider string `toml:"provider" json:"provider"`
	// ConfigFile names the .conf, relative to <data_dir>/wireguard, or as an
	// absolute path. Relative is the normal case: it keeps the key inside a
	// directory Hydra creates 0700.
	ConfigFile string `toml:"config_file" json:"config_file"`
	// Device overrides the interface name. Empty derives it from the engine id,
	// which is what makes two engines on one host land on two interfaces
	// without anyone having to think about it.
	Device string `toml:"device" json:"device"`
	// PortForward: "auto", "manual", "off", or empty for the provider's
	// default. Empty is the useful case -- picking Proton should be enough --
	// and the explicit values exist for the provider we guessed wrong about.
	PortForward string `toml:"port_forward" json:"port_forward"`
	// ManualPort is the port for PortForward = "manual": one the provider
	// assigned through its web panel, which nothing here can discover.
	ManualPort int `toml:"manual_port" json:"manual_port"`
	// RouteTable overrides the routing table this tunnel's routes go into.
	// Empty derives it, and there is almost never a reason to set it: it exists
	// for a host that already uses the derived number for something else.
	RouteTable int `toml:"route_table" json:"route_table"`
}

// Port-forwarding modes.
const (
	PortForwardAuto   = "auto"
	PortForwardManual = "manual"
	PortForwardOff    = "off"
)

// Validate refuses a configuration that cannot work, at load time rather than
// three layers down at tunnel setup.
func (w *WireGuardConfig) Validate() error {
	if w == nil || !w.Enabled {
		return nil
	}
	if strings.TrimSpace(w.ConfigFile) == "" {
		return fmt.Errorf("wireguard is enabled but no config_file is set")
	}
	switch strings.ToLower(strings.TrimSpace(w.PortForward)) {
	case "", PortForwardAuto, PortForwardManual, PortForwardOff:
	default:
		return fmt.Errorf("port_forward %q is not one of auto, manual, off", w.PortForward)
	}
	if strings.EqualFold(w.PortForward, PortForwardManual) && (w.ManualPort <= 0 || w.ManualPort > 65535) {
		return fmt.Errorf("port_forward is manual but manual_port %d is not a port", w.ManualPort)
	}
	if strings.Contains(w.ConfigFile, "..") {
		// The name is joined onto a directory. Without this, a config file
		// could name any file on the host and Hydra would read it as a tunnel
		// key -- and report its parse error, contents included, to whoever
		// asked.
		return fmt.Errorf("config_file %q must not climb out of the wireguard directory", w.ConfigFile)
	}
	return nil
}

// ConfigPath resolves the .conf against the directory reserved for them.
func (w *WireGuardConfig) ConfigPath(dataDir string) string {
	if w == nil {
		return ""
	}
	name := strings.TrimSpace(w.ConfigFile)
	if name == "" {
		return ""
	}
	if filepath.IsAbs(name) {
		return name
	}
	return filepath.Join(WireGuardDir(dataDir), name)
}

// WireGuardDir is where uploaded .conf files live. Created 0700 by the code
// that writes into it: it holds private keys.
func WireGuardDir(dataDir string) string { return filepath.Join(dataDir, "wireguard") }
