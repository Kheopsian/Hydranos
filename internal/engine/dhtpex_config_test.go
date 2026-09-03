package engine

import (
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// The keys are only worth anything if they reach the engine JSON. Both were
// hardcoded to true here for as long as Typhon ignored them, so this asserts
// the wiring by driving it both ways rather than by reading the struct.
func TestEngineConfigCarriesTheConfiguredPeerSources(t *testing.T) {
	for _, tc := range []struct{ dht, pex bool }{{true, true}, {false, false}, {true, false}, {false, true}} {
		sc := &config.SessionConfig{EnableDHT: tc.dht, EnablePEX: tc.pex}
		for name, ec := range map[string]EngineConfig{
			"hoard": BuildHoardConfig(sc, "/tmp"),
			"race":  BuildRaceConfig(sc, "/tmp"),
		} {
			if ec.DHTEnabled != tc.dht || ec.PEXEnabled != tc.pex {
				t.Errorf("%s: config says dht=%v pex=%v, engine JSON says dht=%v pex=%v",
					name, tc.dht, tc.pex, ec.DHTEnabled, ec.PEXEnabled)
			}
		}
	}
}
