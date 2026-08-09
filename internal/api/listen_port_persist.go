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
		return fmt.Errorf("unknown engine role %q", role)
	}
	if port <= 0 || port > 65535 {
		return fmt.Errorf("listen port %d out of range (1-65535)", port)
	}

	configWriteMu.Lock()
	defer configWriteMu.Unlock()

	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		return fmt.Errorf("read config: %w", err)
	}
	doc, err := config.SetTOMLValue(string(data), role, "listen_port", strconv.Itoa(port))
	if err != nil {
		return fmt.Errorf("set [%s] listen_port: %w", role, err)
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
