package api

import (
	"net/http"
	"net/http/httptest"
	"testing"
)

// qbitStub mimics the handful of qBittorrent WebUI endpoints the importer uses.
func qbitStub() *httptest.Server {
	mux := http.NewServeMux()
	mux.HandleFunc("/api/v2/auth/login", func(w http.ResponseWriter, r *http.Request) {
		_ = r.ParseForm()
		if r.FormValue("username") == "admin" && r.FormValue("password") == "secret" {
			w.Write([]byte("Ok."))
			return
		}
		w.Write([]byte("Fails."))
	})
	mux.HandleFunc("/api/v2/torrents/info", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`[
			{"hash":"aaa","name":"Movie A","save_path":"/downloads/movies","category":"movies","progress":1.0,"size":100,"uploaded":500,"added_on":1600000000},
			{"hash":"bbb","name":"Show B","save_path":"/downloads/tv","category":"tv","progress":0.42,"size":200,"uploaded":10,"added_on":1610000000},
			{"hash":"ccc","name":"Loose C","save_path":"/downloads","category":"","progress":1.0,"size":50,"uploaded":7,"added_on":1590000000}
		]`))
	})
	mux.HandleFunc("/api/v2/torrents/categories", func(w http.ResponseWriter, r *http.Request) {
		w.Header().Set("Content-Type", "application/json")
		w.Write([]byte(`{"movies":{"name":"movies","savePath":"/downloads/movies"},"tv":{"name":"tv","savePath":"/downloads/tv"}}`))
	})
	mux.HandleFunc("/api/v2/torrents/export", func(w http.ResponseWriter, r *http.Request) {
		w.Write([]byte("d4:infod6:lengthi100eee")) // fake .torrent bytes
	})
	return httptest.NewServer(mux)
}

func TestQbitClientFlow(t *testing.T) {
	srv := qbitStub()
	defer srv.Close()

	cl, err := newQbitClient(srv.URL)
	if err != nil {
		t.Fatalf("newQbitClient: %v", err)
	}
	if err := cl.login("admin", "wrong"); err == nil {
		t.Fatal("expected login failure with bad password")
	}
	if err := cl.login("admin", "secret"); err != nil {
		t.Fatalf("login: %v", err)
	}
	ts, err := cl.torrentsInfo()
	if err != nil || len(ts) != 3 {
		t.Fatalf("torrentsInfo: %v len=%d", err, len(ts))
	}
	if ts[0].Uploaded != 500 || ts[1].Progress != 0.42 {
		t.Fatalf("field parse mismatch: %+v", ts)
	}
	cats, err := cl.categories()
	if err != nil || cats["movies"].SavePath != "/downloads/movies" {
		t.Fatalf("categories: %v %+v", err, cats)
	}
	data, err := cl.exportTorrent("aaa")
	if err != nil || len(data) == 0 {
		t.Fatalf("export: %v", err)
	}
}

func TestRemapPath(t *testing.T) {
	m := map[string]string{"/downloads": "/data", "/downloads/movies": "/data/films"}
	cases := map[string]string{
		"/downloads/movies/x": "/data/films/x", // longest prefix wins
		"/downloads/tv/y":     "/data/tv/y",
		"/other/z":            "/other/z", // no match unchanged
	}
	for in, want := range cases {
		if got := remapPath(in, m); got != want {
			t.Fatalf("remapPath(%q)=%q want %q", in, got, want)
		}
	}
}

func TestProvenanceRoundTrip(t *testing.T) {
	dir := t.TempDir()
	if _, ok := loadProvenance(dir); ok {
		t.Fatal("expected no provenance initially")
	}
	p := provenance{SourceClient: "qBittorrent", SourceDate: 123, CarriedUploadedBytes: 999, ImportedCount: 42}
	if err := saveProvenance(dir, p); err != nil {
		t.Fatalf("save: %v", err)
	}
	got, ok := loadProvenance(dir)
	if !ok || got != p {
		t.Fatalf("roundtrip mismatch: %+v ok=%v", got, ok)
	}
}
