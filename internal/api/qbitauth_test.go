package api

import (
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"testing"

	"github.com/gin-gonic/gin"
	"golang.org/x/crypto/bcrypt"

	"github.com/Kheopsian/hydra/internal/config"
)

// qbitAuthFixture builds a server carrying the shim's real route table, with
// the credentials the guard checks against.
func qbitAuthFixture(t *testing.T, apiKey, user, password string) *Server {
	t.Helper()
	gin.SetMode(gin.TestMode)

	cfg := &config.HydraConfig{}
	cfg.Daemon.APIKey = apiKey
	if password != "" {
		h, err := bcrypt.GenerateFromPassword([]byte(password), bcrypt.MinCost)
		if err != nil {
			t.Fatal(err)
		}
		cfg.Auth.Username = user
		cfg.Auth.PasswordHash = string(h)
	}

	s := &Server{router: gin.New(), config: cfg, qbitSessions: newQbitSessions()}
	// The real registration, not a stand-in: the finding was that the guard was
	// never mounted, so a fixture that mounts qbitAuth by hand would stay green
	// through exactly the regression these tests exist to catch.
	s.registerQbitRoutes()
	return s
}

// guarded is a shim route that touches no engine, so an unauthenticated call
// stops at the middleware and an authenticated one still answers.
const guarded = "/api/v2/app/version"

func do(s *Server, req *http.Request) *httptest.ResponseRecorder {
	w := httptest.NewRecorder()
	s.router.ServeHTTP(w, req)
	return w
}

func loginReq(user, pass string) *http.Request {
	form := url.Values{"username": {user}, "password": {pass}}
	r := httptest.NewRequest(http.MethodPost, "/api/v2/auth/login", strings.NewReader(form.Encode()))
	r.Header.Set("Content-Type", "application/x-www-form-urlencoded")
	return r
}

// The finding: /api/v2 carried no middleware at all, so anyone who reached the
// port reached torrents/delete?deleteFiles=true. The login endpoint set a
// constant cookie nothing ever read, which is what made it look guarded.
func TestQbitShimRefusesAnUnauthenticatedCall(t *testing.T) {
	s := qbitAuthFixture(t, "real-key", "the maintainer", "hunter2")

	w := do(s, httptest.NewRequest(http.MethodPost, guarded, nil))
	if w.Code != http.StatusForbidden {
		t.Fatalf("an anonymous call reached a shim handler: got %d, want 403", w.Code)
	}
}

// A constant, never-verified cookie must not be a key any more.
func TestQbitShimRejectsTheOldConstantCookie(t *testing.T) {
	s := qbitAuthFixture(t, "real-key", "the maintainer", "hunter2")

	r := httptest.NewRequest(http.MethodPost, guarded, nil)
	r.AddCookie(&http.Cookie{Name: "SID", Value: "hydra-session-token"})
	if w := do(s, r); w.Code != http.StatusForbidden {
		t.Fatalf("the old constant SID still authenticates: got %d, want 403", w.Code)
	}
}

func TestQbitShimAcceptsASessionFromLogin(t *testing.T) {
	s := qbitAuthFixture(t, "real-key", "the maintainer", "hunter2")

	w := do(s, loginReq("the maintainer", "hunter2"))
	if body := w.Body.String(); body != "Ok." {
		t.Fatalf("login body = %q, want %q", body, "Ok.")
	}
	var sid string
	for _, c := range w.Result().Cookies() {
		if c.Name == "SID" {
			sid = c.Value
		}
	}
	if sid == "" {
		t.Fatal("login issued no SID cookie")
	}
	if sid == "hydra-session-token" {
		t.Fatal("login still issues the old constant token")
	}

	r := httptest.NewRequest(http.MethodPost, guarded, nil)
	r.AddCookie(&http.Cookie{Name: "SID", Value: sid})
	if w := do(s, r); w.Code != http.StatusOK {
		t.Fatalf("a logged-in client was refused: got %d, want 200", w.Code)
	}
}

func TestQbitShimRejectsBadCredentials(t *testing.T) {
	s := qbitAuthFixture(t, "real-key", "the maintainer", "hunter2")

	w := do(s, loginReq("the maintainer", "wrong"))
	if body := w.Body.String(); body != "Fails." {
		t.Fatalf("login body = %q, want %q", body, "Fails.")
	}
	if len(w.Result().Cookies()) != 0 {
		t.Fatal("a failed login handed out a cookie")
	}
}

// autobrr and cross-seed are often configured with the daemon API key rather
// than a login, and the native API already accepts it.
func TestQbitShimAcceptsTheDaemonAPIKey(t *testing.T) {
	s := qbitAuthFixture(t, "real-key", "the maintainer", "hunter2")

	r := httptest.NewRequest(http.MethodPost, guarded, nil)
	r.Header.Set("X-Api-Key", "real-key")
	if w := do(s, r); w.Code != http.StatusOK {
		t.Fatalf("X-Api-Key was refused: got %d, want 200", w.Code)
	}

	r = httptest.NewRequest(http.MethodPost, guarded, nil)
	r.Header.Set("X-Api-Key", "wrong-key")
	if w := do(s, r); w.Code != http.StatusForbidden {
		t.Fatalf("a wrong X-Api-Key passed: got %d, want 403", w.Code)
	}
}

// The shim and the native API must agree about whether this instance is open,
// so the dev-mode escape hatch is the same condition apiKeyAuth uses.
func TestQbitShimMatchesTheNativeDevModeEscapeHatch(t *testing.T) {
	open := qbitAuthFixture(t, "change-me-in-production", "the maintainer", "hunter2")
	if w := do(open, httptest.NewRequest(http.MethodPost, guarded, nil)); w.Code != http.StatusOK {
		t.Fatalf("dev mode should stay open as it does on /api: got %d", w.Code)
	}

	// Default key but no admin account: apiKeyAuth still demands the key, and
	// so must the shim -- otherwise an unconfigured install serves everything.
	unconfigured := qbitAuthFixture(t, "change-me-in-production", "", "")
	if w := do(unconfigured, httptest.NewRequest(http.MethodPost, guarded, nil)); w.Code != http.StatusForbidden {
		t.Fatalf("an unconfigured install served the shim anonymously: got %d, want 403", w.Code)
	}
}
