package engine

import (
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"testing"
)

// An http.Transport literal zero-values every timeout, and zero means "no
// limit". Only http.DefaultTransport sets them, and nothing here inherits from
// it. A dial with no ceiling does not just lose its own request: net/http
// purges Transport.dialsInProgress from the front only, so one stuck dial pins
// every wantConn queued behind it until the process dies. This guard exists
// because the same omission shipped in five separate places.
func TestEveryTransportLiteralBoundsItsHandshake(t *testing.T) {
	root := repoRoot(t)
	lit := regexp.MustCompile(`&http\.Transport\{`)

	err := filepath.Walk(root, func(path string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}
		if info.IsDir() {
			switch info.Name() {
			case ".git", "vendor", "node_modules", "web", "typhon-engine":
				return filepath.SkipDir
			}
			return nil
		}
		if !strings.HasSuffix(path, ".go") || strings.HasSuffix(path, "_test.go") {
			return nil
		}
		src, err := os.ReadFile(path)
		if err != nil {
			return err
		}
		for _, loc := range lit.FindAllStringIndex(string(src), -1) {
			// Look at the literal's body: from the brace to the matching close.
			body := transportBody(string(src)[loc[1]:])
			if !strings.Contains(body, "TLSHandshakeTimeout") {
				rel, _ := filepath.Rel(root, path)
				t.Errorf("%s: &http.Transport{...} with no TLSHandshakeTimeout -- an unbounded dial pins every wantConn queued behind it", rel)
			}
		}
		return nil
	})
	if err != nil {
		t.Fatal(err)
	}
}

// transportBody returns the text up to the brace that closes the literal.
func transportBody(s string) string {
	depth := 1
	for i, r := range s {
		switch r {
		case '{':
			depth++
		case '}':
			depth--
			if depth == 0 {
				return s[:i]
			}
		}
	}
	return s
}

func repoRoot(t *testing.T) string {
	t.Helper()
	dir, err := os.Getwd()
	if err != nil {
		t.Fatal(err)
	}
	for {
		if _, err := os.Stat(filepath.Join(dir, "go.mod")); err == nil {
			return dir
		}
		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("go.mod not found above the test directory")
		}
		dir = parent
	}
}
