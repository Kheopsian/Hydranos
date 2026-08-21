package api

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"testing"

	"github.com/gin-gonic/gin"
)

// catHoard answers the two listing calls and records which one was used. The
// embedded interface supplies the rest of the method set; anything else the
// handler reaches for would nil-panic and show up as a failure rather than
// silently passing.
type catHoard struct {
	HoardEngine
	all       []map[string]interface{}
	fullCalls int
	catCalls  int
}

func (h *catHoard) GetTorrentList() []map[string]interface{} {
	h.fullCalls++
	return h.all
}

func (h *catHoard) GetTorrentListInCategory(cat string) []map[string]interface{} {
	h.catCalls++
	out := make([]map[string]interface{}, 0, 4)
	for _, t := range h.all {
		if c, _ := t["category"].(string); c == cat {
			out = append(out, t)
		}
	}
	return out
}

func row(hash, cat string) map[string]interface{} {
	return map[string]interface{}{
		"info_hash": hash, "name": hash, "state": "seeding",
		"progress": 1.0, "category": cat, "total_size": int64(1),
	}
}

// The *arr stack polls this endpoint once per category. Answering must not
// require building a row for every torrent in the catalogue: at 196k torrents
// the categories they ask for hold ~2% of it, and the discarded 98% was the
// single largest allocator in the process.
func TestQbitTorrentsInfoByCategoryDoesNotBuildWholeCatalogue(t *testing.T) {
	gin.SetMode(gin.TestMode)

	h := &catHoard{all: []map[string]interface{}{
		row("aaa", "movies"), row("bbb", "series"), row("ccc", "movies"), row("ddd", ""),
	}}
	s := &Server{hoardEngine: h}
	r := gin.New()
	r.GET("/torrents/info", s.qbitTorrentsInfo)

	InvalidateQbitSnapshot()
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/torrents/info?category=movies", nil))
	if w.Code != http.StatusOK {
		t.Fatalf("status = %d, want 200", w.Code)
	}

	var got []map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
		t.Fatalf("body: %v (%s)", err, w.Body.String())
	}
	if len(got) != 2 {
		t.Fatalf("got %d rows, want the 2 in category movies: %s", len(got), w.Body.String())
	}
	for _, g := range got {
		if c, _ := g["category"].(string); c != "movies" {
			t.Errorf("row leaked from category %q", c)
		}
	}
	if h.fullCalls != 0 {
		t.Errorf("built the whole catalogue %d time(s) to answer a category query", h.fullCalls)
	}
	if h.catCalls == 0 {
		t.Error("never asked for the category")
	}
}

// An unfiltered listing must still return everything.
func TestQbitTorrentsInfoUnfilteredStillReturnsAll(t *testing.T) {
	gin.SetMode(gin.TestMode)

	h := &catHoard{all: []map[string]interface{}{
		row("aaa", "movies"), row("bbb", "series"), row("ccc", ""),
	}}
	s := &Server{hoardEngine: h}
	r := gin.New()
	r.GET("/torrents/info", s.qbitTorrentsInfo)

	InvalidateQbitSnapshot()
	w := httptest.NewRecorder()
	r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/torrents/info", nil))

	var got []map[string]interface{}
	if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
		t.Fatalf("body: %v (%s)", err, w.Body.String())
	}
	if len(got) != 3 {
		t.Fatalf("got %d rows, want 3: %s", len(got), w.Body.String())
	}
	if h.catCalls != 0 {
		t.Errorf("used the category path for an unfiltered listing (%d calls)", h.catCalls)
	}
}

// Two categories must not share a cache slot.
func TestQbitCategoryScopesAreCachedSeparately(t *testing.T) {
	gin.SetMode(gin.TestMode)

	h := &catHoard{all: []map[string]interface{}{row("aaa", "movies"), row("bbb", "series")}}
	s := &Server{hoardEngine: h}
	r := gin.New()
	r.GET("/torrents/info", s.qbitTorrentsInfo)

	InvalidateQbitSnapshot()
	for _, tc := range []struct{ cat, want string }{{"movies", "aaa"}, {"series", "bbb"}} {
		w := httptest.NewRecorder()
		r.ServeHTTP(w, httptest.NewRequest(http.MethodGet, "/torrents/info?category="+tc.cat, nil))
		var got []map[string]interface{}
		if err := json.Unmarshal(w.Body.Bytes(), &got); err != nil {
			t.Fatalf("%s: %v", tc.cat, err)
		}
		if len(got) != 1 {
			t.Fatalf("%s: got %d rows, want 1: %s", tc.cat, len(got), w.Body.String())
		}
		if h, _ := got[0]["hash"].(string); h != tc.want {
			if n, _ := got[0]["name"].(string); n != tc.want {
				t.Errorf("%s: served another category's cached rows (%s)", tc.cat, w.Body.String())
			}
		}
	}
}
