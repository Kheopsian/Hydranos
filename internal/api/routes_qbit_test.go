package api

import (
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/gin-gonic/gin"
)

// An empty torrent listing must come back as [], not null. cross-seed
// dereferences the parsed body directly (torrents.find(...)), so a null body
// throws in the client rather than reading as "no torrents". With no engines
// attached -- the shape of the boot window, before state is restored -- the
// handler used to hand gin a nil slice, which marshals to null.
func TestQbitTorrentsInfoEmptyIsArrayNotNull(t *testing.T) {
	gin.SetMode(gin.TestMode)

	s := &Server{}
	r := gin.New()
	r.GET("/torrents/info", s.qbitTorrentsInfo)

	for _, target := range []string{
		"/torrents/info",
		"/torrents/info?hashes=8feaa9af0000000000000000000000000000dead",
	} {
		w := httptest.NewRecorder()
		r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, target, nil))

		if w.Code != http.StatusOK {
			t.Fatalf("%s: status = %d, want 200", target, w.Code)
		}
		if got := w.Body.String(); got != "[]" {
			t.Errorf("%s: body = %q, want %q", target, got, "[]")
		}
	}
}
