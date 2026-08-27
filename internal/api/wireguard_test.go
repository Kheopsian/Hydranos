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

// A managed tunnel is a mode of its own. Reading it as "direct" put the
// interface and the port on screen as editable fields, when both are written
// by Hydra at every boot.
func TestManagedTunnelIsItsOwnMode(t *testing.T) {
	race := map[string]interface{}{
		"listen_port": int64(16171),
		"wireguard":   map[string]interface{}{"enabled": true, "provider": "proton"},
	}
	hoard := map[string]interface{}{"listen_port": int64(16172)}
	if got := detectNetMode(race, hoard); got != netModeWireGuard {
		t.Errorf("mode = %q, want %q", got, netModeWireGuard)
	}
	// Off, it is direct again and nothing else changes.
	race["wireguard"] = map[string]interface{}{"enabled": false}
	if got := detectNetMode(race, hoard); got != netModeDirect {
		t.Errorf("with the tunnel off, mode = %q, want %q", got, netModeDirect)
	}
	// And a node with no wireguard key at all is untouched by any of this.
	if got := detectNetMode(map[string]interface{}{}, map[string]interface{}{}); got != netModeDirect {
		t.Errorf("a plain node reads as %q", got)
	}
}

// The mode picker promises that choosing one clears the others. A tunnel left
// enabled would keep being built at every boot while the page says direct.
func TestLeavingTheModeSwitchesTheTunnelsOff(t *testing.T) {
	doc := "[race]\nlisten_port = 16171\n\n[race.wireguard]\nenabled = true\nprovider = \"proton\"\n\n[hoard]\nlisten_port = 16172\n"
	out, err := DisableWireGuardTunnels(doc, []string{"race", "hoard"})
	if err != nil {
		t.Fatal(err)
	}
	var cfg config.HydraConfig
	if err := toml.Unmarshal([]byte(out), &cfg); err != nil {
		t.Fatalf("the edited config no longer parses: %v\n%s", err, out)
	}
	if cfg.Race.WireGuard == nil || cfg.Race.WireGuard.Enabled {
		t.Errorf("the race tunnel is still on: %+v\n%s", cfg.Race.WireGuard, out)
	}
	if cfg.Race.ListenPort != 16171 {
		t.Errorf("the edit clobbered the rest of the section: %d", cfg.Race.ListenPort)
	}
	// The provider is left where it is: turning a tunnel off is not the same
	// as forgetting which provider it was, and the operator may turn it back on.
	if cfg.Race.WireGuard.Provider != "proton" {
		t.Errorf("switching off also erased the provider: %+v", cfg.Race.WireGuard)
	}
}

// TOML accepts [race.wireguard] and dotted wireguard.x keys inside [race], and
// REJECTS a file carrying both. Two writers each picking a spelling is how a
// config stops parsing, so the assignment endpoint must write the same table
// the mode save reads.
func TestOneSpellingForAnEnginesTunnel(t *testing.T) {
	doc := "[race]\nlisten_port = 16171\n"
	out, err := config.SetTOMLTable(doc, "race.wireguard", plainKeys(wireGuardKeys(wgEngineReq{
		EngineID: "race", Enabled: true, Provider: "proton", ConfigFile: "a.conf",
	})))
	if err != nil {
		t.Fatal(err)
	}
	if strings.Contains(out, "wireguard.enabled") {
		t.Errorf("the dotted spelling leaked into the file beside the table:\n%s", out)
	}
	var cfg config.HydraConfig
	if err := toml.Unmarshal([]byte(out), &cfg); err != nil {
		t.Fatalf("does not parse: %v\n%s", err, out)
	}
	if cfg.Race.WireGuard == nil || !cfg.Race.WireGuard.Enabled {
		t.Errorf("read back %+v\n%s", cfg.Race.WireGuard, out)
	}
	// And what the mode detector reads must agree with what was written.
	m, err := config.ParseTOMLMap([]byte(out))
	if err != nil {
		t.Fatal(err)
	}
	if !wireGuardEnabledIn(sectionOf(m, "race")) {
		t.Error("the mode detector cannot see the tunnel the assignment endpoint wrote")
	}
}
