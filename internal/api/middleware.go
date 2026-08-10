package api

import (
	"net/http"

	"github.com/gin-gonic/gin"
)

// apiKeyAuth returns a Gin middleware that verifies the X-Api-Key header.
// If the configured API key is the default "change-me-in-production", all
// requests are allowed through without checking (development mode).
func (s *Server) apiKeyAuth() gin.HandlerFunc {
	return func(c *gin.Context) {
		expectedKey := s.config.Daemon.APIKey

		// Skip auth if using the default key (dev mode). Not while the instance
		// still has no admin account: an unconfigured install carrying a legacy
		// placeholder key would otherwise serve its whole API to anyone.
		if expectedKey == "change-me-in-production" && s.config.Auth.PasswordHash != "" {
			c.Next()
			return
		}

		providedKey := c.GetHeader("X-Api-Key")
		if providedKey == "" {
			// Also check query parameter as fallback
			providedKey = c.Query("apikey")
		}

		if providedKey != expectedKey {
			c.AbortWithStatusJSON(http.StatusUnauthorized, gin.H{
				"error": "Invalid or missing API key",
			})
			return
		}

		c.Next()
	}
}
