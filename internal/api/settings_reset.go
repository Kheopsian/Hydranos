package api

import (
	"fmt"
	"net/http"
	"os"
	"strconv"
	"time"

	"github.com/gin-gonic/gin"

	hydra "github.com/Kheopsian/hydra"
	"github.com/Kheopsian/hydra/internal/config"
)

// Resetting to defaults must not lock the operator out of their own daemon.
// Four values are carried across, and they are the ones whose loss cannot be
// undone from the UI: the credentials, the API key every client already holds,
// the token the fronts dialling this node's data-plane already hold, and the
// directory the data lives in. Everything else goes back to what a fresh
// install ships, which is the point of the button.
var resetPreserved = []struct{ section, key string }{
	{"auth", "username"},
	{"auth", "password_hash"},
	{"daemon", "api_key"},
	{"daemon", "agent_token"},
	{"daemon", "data_dir"},
}

func (s *Server) handleSettingsReset(c *gin.Context) {
	configWriteMu.Lock()
	defer configWriteMu.Unlock()

	path := s.settingsFilePath()
	current, err := os.ReadFile(path)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	live, err := config.ParseTOMLMap(current)
	if err != nil {
		// A config we cannot read is exactly when someone reaches for this
		// button, so carry on with the defaults rather than refusing.
		live = map[string]interface{}{}
	}

	doc := hydra.DefaultConfigTOML
	kept := []string{}
	for _, p := range resetPreserved {
		sec := sectionOf(live, p.section)
		val, ok := sec[p.key]
		if !ok {
			continue
		}
		var lit string
		switch v := val.(type) {
		case string:
			if v == "" {
				continue
			}
			lit = strconv.Quote(v)
		default:
			continue
		}
		next, err := config.SetTOMLTable(doc, p.section, [][2]string{{p.key, lit}})
		if err != nil {
			continue
		}
		doc = next
		kept = append(kept, p.section+"."+p.key)
	}

	if _, err := config.ParseTOMLMap([]byte(doc)); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "the rebuilt config does not parse: " + err.Error()})
		return
	}
	if err := config.ValidateTyped([]byte(doc)); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "the rebuilt config would not load: " + err.Error()})
		return
	}

	// Kept out of the way of the ordinary .bak-settings, which the next save
	// would overwrite: this is the copy someone comes looking for after a reset
	// they regret.
	backup := fmt.Sprintf("%s.bak-reset-%d", path, time.Now().Unix())
	if err := os.WriteFile(backup, current, 0o644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "refusing to reset without a backup: " + err.Error()})
		return
	}
	tmp := path + ".tmp"
	if err := os.WriteFile(tmp, []byte(doc), 0o644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	if err := os.Rename(tmp, path); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusOK, gin.H{
		"status": "ok", "backup": backup, "preserved": kept,
		"restart_required": true,
	})
}
