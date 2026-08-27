package api

import (
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/pelletier/go-toml/v2"
)

// The round trip that matters: what the handler writes must be what the daemon
// reads back. A dotted key that lands in the wrong table produces a config
// that parses, saves, reports success -- and leaves the tunnel off.
func TestWireGuardKeysSurviveTheTOMLRoundTrip(t *testing.T) {
	doc := "[race]\nlisten_port = 16171\n\n[hoard]\nlisten_port = 16172\n"
	out, err := config.SetTOMLTable(doc, "race", wireGuardKeys(wgEngineReq{
		EngineID: "race", Enabled: true, Provider: "proton",
		ConfigFile: "proton-fr.conf", PortForward: "auto",
	}))
	if err != nil {
		t.Fatal(err)
	}
	var cfg config.HydraConfig
	if err := toml.Unmarshal([]byte(out), &cfg); err != nil {
		t.Fatalf("the edited config no longer parses: %v\n%s", err, out)
	}
	w := cfg.Race.WireGuard
	if w == nil {
		t.Fatalf("the wireguard block did not land in [race]:\n%s", out)
	}
	if !w.Enabled || w.Provider != "proton" || w.ConfigFile != "proton-fr.conf" || w.PortForward != "auto" {
		t.Errorf("read back %+v", w)
	}
	// The other engine must be untouched: a save for one engine that quietly
	// rewrites the other is how two engines end up on one tunnel.
	if cfg.Hoard.WireGuard != nil {
		t.Errorf("the hoard engine grew a tunnel it never asked for: %+v", cfg.Hoard.WireGuard)
	}
	if cfg.Race.ListenPort != 16171 {
		t.Errorf("the edit clobbered listen_port: %d", cfg.Race.ListenPort)
	}
}

// Turning a tunnel off must write every key, not just enabled=false: a
// leftover provider and config_file next to a disabled tunnel is a loaded gun
// for whoever flips the switch back without reading the rest.
func TestDisablingWritesTheWholeBlock(t *testing.T) {
	kv := wireGuardKeys(wgEngineReq{EngineID: "race", Enabled: false})
	var seen []string
	for _, p := range kv {
		seen = append(seen, p[0])
	}
	for _, want := range []string{"wireguard.enabled", "wireguard.provider", "wireguard.config_file", "wireguard.port_forward", "wireguard.manual_port"} {
		if !strings.Contains(strings.Join(seen, ","), want) {
			t.Errorf("%q is not written when the tunnel is turned off: %v", want, seen)
		}
	}
}

// A file name is joined onto a directory. Anything that can climb out of it
// would be read as a tunnel config and its parse error reported back.
func TestConfigFileNameIsReducedToItsBase(t *testing.T) {
	kv := wireGuardKeys(wgEngineReq{EngineID: "race", Enabled: true, ConfigFile: "../../etc/shadow"})
	for _, p := range kv {
		if p[0] == "wireguard.config_file" {
			if strings.Contains(p[1], "..") || strings.Contains(p[1], "/") {
				t.Errorf("a path escaped into config_file: %s", p[1])
			}
		}
	}
}

func TestValidateRejectsManualWithoutAPort(t *testing.T) {
	w := &config.WireGuardConfig{Enabled: true, ConfigFile: "a.conf", PortForward: "manual"}
	if err := w.Validate(); err == nil {
		t.Error("manual port forwarding with no port was accepted: the engine would take no incoming peers and say nothing")
	}
	w.ManualPort = 51413
	if err := w.Validate(); err != nil {
		t.Errorf("a valid manual setup was refused: %v", err)
	}
}

// The bug this catches was live: a config using the classic [race]/[hoard]
// sections has s.config.Engines EMPTY, so a guard that walks it protects
// nothing while looking like it does.
func TestResolvedEnginesSeesClassicRaceAndHoardSections(t *testing.T) {
	cfg := &config.HydraConfig{}
	cfg.Race.ListenPort = 16171
	cfg.Race.WireGuard = &config.WireGuardConfig{Enabled: true, ConfigFile: "proton-a.conf", Provider: "proton"}
	cfg.Hoard.ListenPort = 16172
	if len(cfg.Engines) != 0 {
		t.Fatal("this test is pointless unless [[engine]] blocks are absent")
	}
	s := &Server{config: cfg}
	found := ""
	for _, ec := range s.resolvedEngines() {
		if w := ec.SessionConfig.WireGuard; w != nil && w.Enabled {
			found = w.ConfigFile
		}
	}
	if found != "proton-a.conf" {
		t.Errorf("the race engine's tunnel is invisible to the API (found %q): its .conf could be deleted while in use", found)
	}
}
