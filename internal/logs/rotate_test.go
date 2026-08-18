package logs

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestRotatingFileRotatesAtLimit(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "hydra.log")

	r, err := newRotatingFile(p, 100, 3) // live + .1 + .2
	if err != nil {
		t.Fatalf("newRotatingFile: %v", err)
	}

	line := []byte(strings.Repeat("x", 40) + "\n") // 41 bytes
	for i := 0; i < 10; i++ {
		if _, err := r.Write(line); err != nil {
			t.Fatalf("write %d: %v", i, err)
		}
	}

	// Nothing beyond the configured generations may survive.
	if _, err := os.Stat(p + ".3"); !os.IsNotExist(err) {
		t.Error("generation .3 exists but maxFiles is 3")
	}
	for _, name := range []string{p, p + ".1", p + ".2"} {
		st, err := os.Stat(name)
		if err != nil {
			t.Fatalf("%s missing: %v", name, err)
		}
		if st.Size() > 100 {
			t.Errorf("%s is %d bytes, over the 100-byte limit", name, st.Size())
		}
	}
}

func TestRotatingFileKeepsWritingAfterRotation(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "hydra.log")

	r, err := newRotatingFile(p, 50, 2)
	if err != nil {
		t.Fatalf("newRotatingFile: %v", err)
	}
	for _, s := range []string{"first\n", strings.Repeat("y", 60) + "\n", "last\n"} {
		if _, err := r.Write([]byte(s)); err != nil {
			t.Fatalf("write %q: %v", s, err)
		}
	}

	b, err := os.ReadFile(p)
	if err != nil {
		t.Fatalf("live file unreadable: %v", err)
	}
	if !strings.Contains(string(b), "last") {
		t.Errorf("live file lost the newest line, got %q", b)
	}
}

func TestRotatingFileSingleGenerationTruncates(t *testing.T) {
	dir := t.TempDir()
	p := filepath.Join(dir, "hydra.log")

	// maxFiles=1 keeps no archive: the file is dropped and restarted, which is
	// still bounded (the point of the exercise) rather than unbounded growth.
	r, err := newRotatingFile(p, 30, 1)
	if err != nil {
		t.Fatalf("newRotatingFile: %v", err)
	}
	for i := 0; i < 5; i++ {
		if _, err := r.Write([]byte(strings.Repeat("z", 20) + "\n")); err != nil {
			t.Fatalf("write %d: %v", i, err)
		}
	}
	if _, err := os.Stat(p + ".1"); !os.IsNotExist(err) {
		t.Error("archive created although maxFiles is 1")
	}
	st, err := os.Stat(p)
	if err != nil {
		t.Fatalf("live file missing: %v", err)
	}
	if st.Size() > 30 {
		t.Errorf("live file is %d bytes, over the 30-byte limit", st.Size())
	}
}

func TestRotatingFileDefaultsOnBadArgs(t *testing.T) {
	dir := t.TempDir()
	r, err := newRotatingFile(filepath.Join(dir, "hydra.log"), 0, 0)
	if err != nil {
		t.Fatalf("newRotatingFile: %v", err)
	}
	if r.maxBytes != defaultMaxLogBytes {
		t.Errorf("maxBytes = %d, want the default %d", r.maxBytes, defaultMaxLogBytes)
	}
	if r.maxFiles != 1 {
		t.Errorf("maxFiles = %d, want 1 (the floor)", r.maxFiles)
	}
}

func TestRotatingFileOnBadPath(t *testing.T) {
	if _, err := newRotatingFile("/definitely/not/a/dir/hydra.log", 100, 2); err == nil {
		t.Error("expected an error for an unopenable path")
	}
}

func TestParseEngineLevelHandlesANSI(t *testing.T) {
	// What the engine actually writes: the level is wrapped in colour codes, so
	// it is never surrounded by plain spaces. This is the regression that filed
	// every engine line as INFO.
	for _, tc := range []struct{ line, want string }{
		{"\x1b[2m2026-08-18T12:47:11Z\x1b[0m \x1b[33m WARN\x1b[0m [peer] handshake failed", "WARN"},
		{"\x1b[2m2026-08-18T12:47:11Z\x1b[0m \x1b[32m INFO\x1b[0m [peer] incoming tcp", "INFO"},
		{"\x1b[31mERROR\x1b[0m disk write failed", "ERROR"},
	} {
		if got := parseEngineLevel(tc.line); got != tc.want {
			t.Errorf("parseEngineLevel(%q) = %q, want %q", tc.line, got, tc.want)
		}
	}
}
