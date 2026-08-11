package api

import (
	"fmt"
	"net"
	"net/http"
	"os"
	"sync/atomic"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"
)

// storageWarning names the network filesystem data_dir sits on, or is empty on
// local disk. Set once at boot, read by the UI so the warning lives somewhere a
// user actually looks — a line in the startup log scrolls away forever.
var storageWarning atomic.Value

// SetStorageWarning records that data_dir is on network storage of this kind.
func SetStorageWarning(kind string) { storageWarning.Store(kind) }

func storageWarningKind() string {
	k, _ := storageWarning.Load().(string)
	return k
}

// handleSetupStatus (public) tells the browser whether an admin account exists
// yet, and surfaces the degraded-storage notice. It is deliberately readable
// without credentials: it leaks nothing an unauthenticated visitor could not
// already infer from a login attempt.
func (s *Server) handleSetupStatus(c *gin.Context) {
	c.JSON(http.StatusOK, gin.H{
		"needs_setup":     s.config.Auth.PasswordHash == "",
		"network_storage": storageWarningKind(),
		"store_repair":    RepairNeeded(),
	})
}

// handleSetupPassword (public, first run only) creates the admin account.
//
// Two guards, because this route hands out the API key. It refuses once a
// password exists — so it cannot be used to take over a configured instance —
// and it only answers callers on the loopback or a private network, so an
// instance port-forwarded to the internet before its owner finished setting it
// up cannot be claimed by whoever finds it first. A remote admin who genuinely
// needs it still has `hydra reset-password`.
func (s *Server) handleSetupPassword(c *gin.Context) {
	if s.config.Auth.PasswordHash != "" {
		c.JSON(http.StatusConflict, gin.H{"error": "admin account already configured"})
		return
	}
	if !isLocalRequest(c.ClientIP()) {
		c.JSON(http.StatusForbidden, gin.H{
			"error": "first-run setup is only allowed from localhost or a private network; " +
				"set the password on the host with: hydra reset-password <new>",
		})
		return
	}
	var req struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if req.Username == "" {
		req.Username = s.config.Auth.Username
	}
	if req.Username == "" {
		req.Username = "admin"
	}
	if len(req.Password) < 8 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "password too short (min 8 chars)"})
		return
	}
	h, err := bcrypt.GenerateFromPassword([]byte(req.Password), bcrypt.DefaultCost)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	// Persist before hot-applying: if the write fails the account stays
	// unconfigured and the user can retry. The old failure mode was the exact
	// opposite — credentials live in memory, nothing on disk, no way back in.
	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	doc, err := config.SetTOMLValue(string(data), "auth", "username", fmt.Sprintf("%q", req.Username))
	if err != nil {
		doc = string(data) // no username line to update: keep the configured one
	}
	doc, err = config.SetTOMLValue(doc, "auth", "password_hash", fmt.Sprintf("%q", string(h)))
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "config has no [auth] password_hash line to update: " + err.Error()})
		return
	}
	if err := os.WriteFile(path, []byte(doc), 0644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	s.config.Auth.Username = req.Username
	s.config.Auth.PasswordHash = string(h)
	c.JSON(http.StatusOK, gin.H{"status": "ok", "api_key": s.config.Daemon.APIKey})
}

// isLocalRequest reports whether the caller is on the loopback or a private
// network (RFC1918 / RFC4193 / link-local).
func isLocalRequest(ip string) bool {
	addr := net.ParseIP(ip)
	if addr == nil {
		return false
	}
	return addr.IsLoopback() || addr.IsPrivate() || addr.IsLinkLocalUnicast()
}
