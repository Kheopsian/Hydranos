package api

import (
	"fmt"
	"net/http"
	"os"

	"github.com/Kheopsian/hydra/internal/config"
	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"
)

// handleLogin (public) verifies username/password against [auth] and, on
// success, returns the daemon API key. The browser stores it and uses it for
// X-Api-Key on every subsequent request — no server-side sessions.
func (s *Server) handleLogin(c *gin.Context) {
	var req struct {
		Username string `json:"username"`
		Password string `json:"password"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	a := s.config.Auth
	if a.PasswordHash == "" {
		c.JSON(http.StatusServiceUnavailable, gin.H{"error": "auth not configured — set [auth] password_hash (run: hydra hash-password <pw>)"})
		return
	}
	if req.Username != a.Username ||
		bcrypt.CompareHashAndPassword([]byte(a.PasswordHash), []byte(req.Password)) != nil {
		c.JSON(http.StatusUnauthorized, gin.H{"error": "invalid credentials"})
		return
	}
	c.JSON(http.StatusOK, gin.H{"api_key": s.config.Daemon.APIKey})
}

// handleSetPassword (authenticated) bcrypt-hashes a new password and persists it
// to [auth] password_hash in the TOML config. Applied hot (no restart).
func (s *Server) handleSetPassword(c *gin.Context) {
	var req struct {
		Password string `json:"password"`
	}
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	if len(req.Password) < 6 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "password too short (min 6 chars)"})
		return
	}
	h, err := bcrypt.GenerateFromPassword([]byte(req.Password), bcrypt.DefaultCost)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	path := s.settingsFilePath()
	data, err := os.ReadFile(path)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	doc, err := config.SetTOMLValue(string(data), "auth", "password_hash", fmt.Sprintf("%q", string(h)))
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "config has no [auth] password_hash line to update: " + err.Error()})
		return
	}
	if err := os.WriteFile(path, []byte(doc), 0644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	s.config.Auth.PasswordHash = string(h) // hot-apply
	c.JSON(http.StatusOK, gin.H{"status": "ok"})
}
