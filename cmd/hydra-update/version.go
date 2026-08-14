package main

import (
	"strconv"
	"strings"
)

type release struct {
	TagName string `json:"tag_name"`
	Assets  []struct {
		Name string `json:"name"`
		URL  string `json:"browser_download_url"`
	} `json:"assets"`
}

// pickAssets finds the Windows archive and its checksum by suffix rather than
// by exact name, so a change to the version string in the filename does not
// silently stop updates working.
func pickAssets(r *release) (zipURL, shaURL string) {
	for _, a := range r.Assets {
		switch {
		case strings.HasSuffix(a.Name, "-windows-amd64.zip"):
			zipURL = a.URL
		case strings.HasSuffix(a.Name, "-windows-amd64.zip.sha256"):
			shaURL = a.URL
		}
	}
	return
}

// newer compares dotted numeric versions. Anything it cannot parse counts as
// newer: better to offer an update that turns out to be redundant than to go
// quiet forever because a tag picked up a suffix.
func newer(latest, current string) bool {
	a, b := parts(clean(latest)), parts(clean(current))
	for i := 0; i < len(a) || i < len(b); i++ {
		var x, y int
		if i < len(a) {
			x = a[i]
		}
		if i < len(b) {
			y = b[i]
		}
		if x != y {
			return x > y
		}
	}
	return false
}

func parts(v string) []int {
	var out []int
	for _, seg := range strings.Split(v, ".") {
		n, err := strconv.Atoi(seg)
		if err != nil {
			return out
		}
		out = append(out, n)
	}
	return out
}

// clean strips the build suffix the daemon carries ("3.64.0-typhon") and any
// leading v, leaving something parts() can read.
func clean(v string) string {
	v = strings.TrimPrefix(strings.TrimSpace(v), "v")
	if i := strings.IndexAny(v, "-+"); i > 0 {
		v = v[:i]
	}
	if v == "" {
		return "an older version"
	}
	return v
}
