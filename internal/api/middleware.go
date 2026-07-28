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

		// Skip auth if using the default key (dev mode)
		if expectedKey == "change-me-in-production" {
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
