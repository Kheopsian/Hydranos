package main

import "testing"

func TestNewer(t *testing.T) {
	cases := []struct {
		latest, current string
		want            bool
	}{
		// The daemon carries a build suffix; the tag carries a leading v.
		{"3.65.0", "3.64.0-typhon", true},
		{"v3.65.0", "3.65.0-typhon", false},
		{"3.64.1", "3.64.0", true},
		{"3.64.0", "3.64.1", false},
		{"4.0.0", "3.99.9", true},
		// Shorter is not older: 3.65 and 3.65.0 are the same release.
		{"3.65", "3.65.0", false},
		{"3.65.1", "3.65", true},
		// Unparseable current: offering a redundant update beats going quiet.
		{"3.65.0", "", true},
		{"3.65.0", "dev", true},
	}
	for _, c := range cases {
		if got := newer(c.latest, c.current); got != c.want {
			t.Errorf("newer(%q, %q) = %v, want %v", c.latest, c.current, got, c.want)
		}
	}
}

func TestClean(t *testing.T) {
	for in, want := range map[string]string{
		"v3.65.0":       "3.65.0",
		"3.65.0-typhon": "3.65.0",
		" 3.65.0 ":      "3.65.0",
		"":              "an older version",
	} {
		if got := clean(in); got != want {
			t.Errorf("clean(%q) = %q, want %q", in, got, want)
		}
	}
}

func TestPickAssets(t *testing.T) {
	r := &release{Assets: []struct {
		Name string `json:"name"`
		URL  string `json:"browser_download_url"`
	}{
		{"hydra-v3.65.0-linux-amd64.tar.gz", "linux"},
		{"hydra-v3.65.0-windows-amd64.zip", "zip"},
		{"hydra-v3.65.0-windows-amd64.zip.sha256", "sha"},
	}}
	// Matched by suffix, not by exact name: a change to the version string in
	// the filename must not silently stop updates working.
	z, s := pickAssets(r)
	if z != "zip" || s != "sha" {
		t.Fatalf("pickAssets = %q, %q; want zip, sha", z, s)
	}
}

func TestPickAssetsMissing(t *testing.T) {
	r := &release{Assets: []struct {
		Name string `json:"name"`
		URL  string `json:"browser_download_url"`
	}{{"hydra-v3.65.0-linux-amd64.tar.gz", "linux"}}}
	if z, _ := pickAssets(r); z != "" {
		t.Fatalf("expected no windows archive, got %q", z)
	}
}
