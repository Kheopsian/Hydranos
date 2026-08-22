package api

import (
	"html/template"
	"net/http/httptest"
	"strings"
	"testing"

	hydraroot "github.com/Kheopsian/hydra"
	"github.com/gin-gonic/gin"
)

func indexBody(t *testing.T, frontOnly bool) string {
	t.Helper()
	gin.SetMode(gin.TestMode)
	tmpl, err := template.ParseFS(hydraroot.WebAssets, "web/templates/*.html")
	if err != nil {
		t.Fatalf("parse templates: %v", err)
	}
	r := gin.New()
	r.SetHTMLTemplate(tmpl)
	s := &Server{frontOnly: frontOnly}
	r.GET("/", s.handleIndex)
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest("GET", "/", nil))
	if w.Code != 200 {
		t.Fatalf("index returned %d", w.Code)
	}
	return w.Body.String()
}

// The controller's own egress is not the fleet's, and the header stat reads as
// if it were.
func TestFrontOnlyIndexHidesExitIP(t *testing.T) {
	if strings.Contains(indexBody(t, true), "header-exit-ip") {
		t.Error("front-only index still renders the exit-IP stat")
	}
}

func TestMonolithIndexShowsExitIP(t *testing.T) {
	if !strings.Contains(indexBody(t, false), "header-exit-ip") {
		t.Error("exit-IP stat is missing outside front-only mode")
	}
}
