package api

import (
	"strings"
	"testing"

	"github.com/Kheopsian/hydra/internal/config"
)

// TestEngineBlocksAreDetected guards the check that stops this page writing
// into a part of the file the daemon ignores. ResolveEngines treats [[engine]]
// blocks as EXCLUSIVE: the moment one exists, [race] and [hoard] are never
// read. This page writes only those two sections, so without the check a save
// on such a node reports success and changes nothing -- not now, not after a
// restart.
func TestEngineBlocksAreDetected(t *testing.T) {
	withBlocks := `
[daemon]
data_dir = "/configs"

[[engine]]
id = "race-0"
role = "race"
listen_port = 16171
`
	m, err := config.ParseTOMLMap([]byte(withBlocks))
	if err != nil {
		t.Fatal(err)
	}
	if !usesEngineBlocks(m) {
		t.Error("a config with [[engine]] blocks was not detected: the network tab would write keys the daemon never reads")
	}

	plain := `
[daemon]
data_dir = "/configs"

[race]
listen_port = 16171

[hoard]
listen_port = 16172
`
	m2, err := config.ParseTOMLMap([]byte(plain))
	if err != nil {
		t.Fatal(err)
	}
	if usesEngineBlocks(m2) {
		t.Error("a plain [race]/[hoard] config was flagged: the tab would refuse to save on the setup it is built for")
	}
}

// The detection must survive whichever shape the TOML parser hands back for an
// array of tables, or it silently answers false and the guard is decorative.
func TestEngineBlockDetectionHandlesBothParserShapes(t *testing.T) {
	for name, m := range map[string]map[string]interface{}{
		"[]map":       {"engine": []map[string]interface{}{{"id": "race-0"}}},
		"[]interface": {"engine": []interface{}{map[string]interface{}{"id": "race-0"}}},
		"empty slice": {"engine": []interface{}{}},
		"absent":      {},
	} {
		got := usesEngineBlocks(m)
		want := strings.HasPrefix(name, "[]map") || name == "[]interface"
		if got != want {
			t.Errorf("%s: usesEngineBlocks = %v, want %v", name, got, want)
		}
	}
}
