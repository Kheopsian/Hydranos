package api

import (
	"fmt"
	"os"
	"strconv"

	"github.com/Kheopsian/hydra/internal/config"
)

// persistListenPort writes a hot-rebound peer port back into the TOML, under
// [race] or [hoard].
//
// The TOML is the single source of truth on purpose: the Config tab renders
// the file straight off disk, so anything that remembers a port elsewhere
// makes the settings screen contradict the Agents screen. Writing here means
// the port is restored at boot for free, since the file is what boot reads.
//
// It shares configWriteMu with the settings save, so a rebind and a settings
// write cannot interleave and lose each other's edits.
func (s *Server) persistListenPort(role string, port int) error {
	if role != "race" && role != "hoard" {
		return fmt.Errorf("unknown agent role %q", role)
	}
	if port <= 0 || port > 65535 {
		return fmt.Errorf("listen port %d out of range (1-65535)", port)
	}

	return s.persistEngineKeys(role, [][2]string{{"listen_port", strconv.Itoa(port)}})
}

// persistDialLimits writes a hot-applied dial governor back into the TOML.
// Nil fields are left untouched in the file, matching what was applied to the
// engine: a request that moved one ceiling must not quietly rewrite the other.
func (s *Server) persistDialLimits(role string, maxDialsPerSec *float64, maxConnections *int) error {
	if role != "race" && role != "hoard" {
		return fmt.Errorf("unknown agent role %q", role)
	}
	var kv [][2]string
	if maxDialsPerSec != nil {
		kv = append(kv, [2]string{"max_dials_per_sec", strconv.FormatFloat(*maxDialsPerSec, 'f', -1, 64)})
	}
	if maxConnections != nil {
		kv = append(kv, [2]string{"max_connections", strconv.Itoa(*maxConnections)})
	}
	if len(kv) == 0 {
		return nil
	}
	return s.persistEngineKeys(role, kv)
}

// persistEngineKeys applies a set of key/value edits to one engine's TOML
// table, under the same lock and the same parse/schema guards as a settings
// save. Ordered pairs rather than a map so the edits are applied
// deterministically and a failure always names the same key first.
func (s *Server) persistEngineKeys(role string, kv [][2]string) error {
	configWriteMu.Lock()
	defer configWriteMu.Unlock()

	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("read config: %w", err)
	}
	doc := string(data)
	for _, pair := range kv {
		doc, err = config.SetTOMLValue(doc, role, pair[0], pair[1])
		if err != nil {
			return fmt.Errorf("set [%s] %s: %w", role, pair[0], err)
		}
	}
	// Same guards as a settings save: never leave a config on disk that the
	// daemon cannot boot from.
	if _, err := config.ParseTOMLMap([]byte(doc)); err != nil {
		return fmt.Errorf("edited config no longer parses: %w", err)
	}
	if err := config.ValidateTyped([]byte(doc)); err != nil {
		return fmt.Errorf("edited config breaks the schema: %w", err)
	}

	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(doc), 0644); err != nil {
		return fmt.Errorf("write config: %w", err)
	}
	return os.Rename(tmp, path)
}
