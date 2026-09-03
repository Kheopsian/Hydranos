package config

import (
	"os"
	"path/filepath"
	"testing"
)

// The upgrade case, and the only one that can break silently: a config written
// before these keys existed must keep DHT and PEX on. Reload decodes the file
// over DefaultConfig, so an absent key keeps the default -- but a plain bool
// zero value is false, so getting the default wrong would switch off peer
// discovery for every existing install without a single error.
func TestPeerSourcesStayOnWhenTheConfigPredatesTheKeys(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "default.toml")
	old := "[daemon]\napi_port = 8199\n\n[race]\nlisten_port = 16171\n\n[hoard]\nlisten_port = 16172\n"
	if err := os.WriteFile(path, []byte(old), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := Reload(path)
	if err != nil {
		t.Fatal(err)
	}
	if !cfg.Race.EnableDHT || !cfg.Race.EnablePEX {
		t.Errorf("race lost its peer sources on upgrade: dht=%v pex=%v", cfg.Race.EnableDHT, cfg.Race.EnablePEX)
	}
	if !cfg.Hoard.EnableDHT || !cfg.Hoard.EnablePEX {
		t.Errorf("hoard lost its peer sources on upgrade: dht=%v pex=%v", cfg.Hoard.EnableDHT, cfg.Hoard.EnablePEX)
	}
}

// And the switch has to actually switch: false in the file must survive the
// merge over a default of true, per engine and per key.
func TestPeerSourcesTurnOffPerEngineAndPerKey(t *testing.T) {
	dir := t.TempDir()
	path := filepath.Join(dir, "default.toml")
	body := "[race]\nenable_dht = false\nenable_pex = true\n\n[hoard]\nenable_dht = true\nenable_pex = false\n"
	if err := os.WriteFile(path, []byte(body), 0o644); err != nil {
		t.Fatal(err)
	}
	cfg, err := Reload(path)
	if err != nil {
		t.Fatal(err)
	}
	if cfg.Race.EnableDHT || !cfg.Race.EnablePEX {
		t.Errorf("race: want dht=false pex=true, got dht=%v pex=%v", cfg.Race.EnableDHT, cfg.Race.EnablePEX)
	}
	if !cfg.Hoard.EnableDHT || cfg.Hoard.EnablePEX {
		t.Errorf("hoard: want dht=true pex=false, got dht=%v pex=%v", cfg.Hoard.EnableDHT, cfg.Hoard.EnablePEX)
	}
}

// "Disable it everywhere" has to reach the remote nodes too, otherwise an
// agent keeps talking to the DHT while the front reports it off.
func TestPeerSourcesReachAnAgentsComposedSession(t *testing.T) {
	cfg := DefaultConfig()
	cfg.Hoard.EnableDHT = false
	cfg.Hoard.EnablePEX = false
	sess, err := cfg.ComposeSession("some-agent", "hoard", "hoard")
	if err != nil {
		t.Fatal(err)
	}
	if sess.EnableDHT || sess.EnablePEX {
		t.Errorf("the front pushed peer sources the config had switched off: dht=%v pex=%v", sess.EnableDHT, sess.EnablePEX)
	}
}
