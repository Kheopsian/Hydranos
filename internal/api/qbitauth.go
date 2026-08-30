package api

import (
	"crypto/rand"
	"crypto/subtle"
	"encoding/hex"
	"net/http"
	"sync"
	"time"

	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"
)

// qbitSessionTTL matches the cookie the shim has always handed out, so a
// long-running autobrr or cross-seed does not have to log in more often than
// it did before.
const qbitSessionTTL = 24 * time.Hour

// qbitSessions holds the SID tokens issued by /api/v2/auth/login.
//
// The shim used to answer every login with the constant cookie
// "hydra-session-token" and read it back nowhere, so /api/v2 was open to
// anyone who could reach the port -- including torrents/delete?deleteFiles=true
// and torrents/add with an arbitrary savepath. It looked like authentication,
// which is the part that made it dangerous.
type qbitSessions struct {
	mu   sync.Mutex
	tok  map[string]time.Time // token -> expiry
	seen time.Time            // last sweep
}

func newQbitSessions() *qbitSessions {
	return &qbitSessions{tok: make(map[string]time.Time)}
}

func (q *qbitSessions) issue() (string, error) {
	var b [32]byte
	if _, err := rand.Read(b[:]); err != nil {
		return "", err
	}
	token := hex.EncodeToString(b[:])

	q.mu.Lock()
	defer q.mu.Unlock()
	now := time.Now()
	// Sweep at most once a minute: the map only grows by one per login.
	if now.Sub(q.seen) > time.Minute {
		for t, exp := range q.tok {
			if now.After(exp) {
				delete(q.tok, t)
			}
		}
		q.seen = now
	}
	q.tok[token] = now.Add(qbitSessionTTL)
	return token, nil
}

func (q *qbitSessions) valid(token string) bool {
	if token == "" {
		return false
	}
	q.mu.Lock()
	defer q.mu.Unlock()
	exp, ok := q.tok[token]
	if !ok {
		return false
	}
	if time.Now().After(exp) {
		delete(q.tok, token)
		return false
	}
	return true
}

func (q *qbitSessions) drop(token string) {
	q.mu.Lock()
	defer q.mu.Unlock()
	delete(q.tok, token)
}

// qbitAuth guards the qBittorrent shim. A caller passes with either a SID
// cookie from /api/v2/auth/login or the daemon API key, so an existing script
// that already carries X-Api-Key keeps working without a login round-trip.
//
// The dev-mode escape hatch is deliberately the same condition as apiKeyAuth on
// the native API: the two surfaces must not disagree about whether this
// instance is open.
func (s *Server) qbitAuth() gin.HandlerFunc {
	return func(c *gin.Context) {
		if s.config.Daemon.APIKey == "change-me-in-production" && s.config.Auth.PasswordHash != "" {
			c.Next()
			return
		}

		key := c.GetHeader("X-Api-Key")
		if key == "" {
			key = c.Query("apikey")
		}
		if key != "" && subtle.ConstantTimeCompare([]byte(key), []byte(s.config.Daemon.APIKey)) == 1 {
			c.Next()
			return
		}

		if sid, err := c.Cookie("SID"); err == nil && s.qbitSessions.valid(sid) {
			c.Next()
			return
		}

		// qBittorrent's own answer to an unauthenticated API call. Clients key
		// off 403 to decide they must log in again.
		c.AbortWithStatus(http.StatusForbidden)
	}
}

// qbitAuthLogin verifies the credentials in [auth] and issues a SID cookie.
// qBittorrent answers 200 with "Ok." or "Fails." rather than a status code, and
// autobrr/cross-seed read the body, so the shim does the same.
func (s *Server) qbitAuthLogin(c *gin.Context) {
	username := c.PostForm("username")
	password := c.PostForm("password")

	a := s.config.Auth
	if a.PasswordHash == "" {
		// No admin account: there is nothing to check a password against.
		// Callers can still reach the shim with the daemon API key.
		c.String(http.StatusOK, "Fails.")
		return
	}
	if subtle.ConstantTimeCompare([]byte(username), []byte(a.Username)) != 1 ||
		bcrypt.CompareHashAndPassword([]byte(a.PasswordHash), []byte(password)) != nil {
		c.String(http.StatusOK, "Fails.")
		return
	}

	token, err := s.qbitSessions.issue()
	if err != nil {
		c.String(http.StatusInternalServerError, "Fails.")
		return
	}
	c.SetCookie("SID", token, int(qbitSessionTTL/time.Second), "/", "", false, true)
	c.String(http.StatusOK, "Ok.")
}

func (s *Server) qbitAuthLogout(c *gin.Context) {
	if sid, err := c.Cookie("SID"); err == nil {
		s.qbitSessions.drop(sid)
	}
	c.SetCookie("SID", "", -1, "/", "", false, true)
	c.String(http.StatusOK, "")
}
