package main

import (
	"errors"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"testing"

	hydraroot "github.com/Kheopsian/hydra"
)

var dataDirRe = regexp.MustCompile(`(?m)^data_dir\s*=\s*(.+)$`)

func readDataDir(t *testing.T, path string) string {
	t.Helper()
	b, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("reading the seeded config: %v", err)
	}
	m := dataDirRe.FindStringSubmatch(string(b))
	if m == nil {
		t.Fatalf("no data_dir in the seeded config:\n%s", b)
	}
	return strings.TrimSpace(m[1])
}

// An explicit --config pointing at nothing is the k8s case: an empty volume,
// a bypassed entrypoint, and a container that used to die before it could
// write anything.
func TestResolveConfigPathSeedsMissingExplicitPath(t *testing.T) {
	dir := filepath.Join(t.TempDir(), "config")
	target := filepath.Join(dir, "default.toml")

	got, seeded := resolveConfigPath(target, true, true)
	if got != target || !seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, true)", got, seeded, target)
	}
	// The parent directory was created with it: an empty mounted volume is
	// enough.
	if _, err := os.Stat(target); err != nil {
		t.Fatalf("nothing was seeded: %v", err)
	}
	// An absolute path takes its own directory as data_dir, like the sed in
	// entrypoint.sh.
	if want := `"` + dir + `"`; readDataDir(t, target) != want {
		t.Errorf("data_dir = %s, want %s", readDataDir(t, target), want)
	}
	st, err := os.Stat(target)
	if err != nil {
		t.Fatal(err)
	}
	if perm := st.Mode().Perm(); perm != 0644 {
		t.Errorf("mode = %o, want 644 (CreateTemp opens 0600)", perm)
	}
}

// A relative path cannot dictate a data directory, so it keeps the portable
// relative one, resolved next to the executable at boot.
func TestResolveConfigPathRelativeKeepsRelativeDataDir(t *testing.T) {
	t.Chdir(t.TempDir())

	got, seeded := resolveConfigPath("hydra.toml", true, true)
	if got != "hydra.toml" || !seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (\"hydra.toml\", true)", got, seeded)
	}
	if want := `"data"`; readDataDir(t, "hydra.toml") != want {
		t.Errorf("data_dir = %s, want %s", readDataDir(t, "hydra.toml"), want)
	}
}

// Seeding must never touch a config that is already there.
func TestResolveConfigPathLeavesAnExistingConfigAlone(t *testing.T) {
	target := filepath.Join(t.TempDir(), "default.toml")
	const mine = "# mine\n[daemon]\ndata_dir = \"/payload\"\n"
	if err := os.WriteFile(target, []byte(mine), 0644); err != nil {
		t.Fatal(err)
	}

	got, seeded := resolveConfigPath(target, true, true)
	if got != target || seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, false)", got, seeded, target)
	}
	b, err := os.ReadFile(target)
	if err != nil {
		t.Fatal(err)
	}
	if string(b) != mine {
		t.Errorf("the existing config was rewritten:\n%s", b)
	}
}

// A directory is a path mistake, not an empty volume: nothing is seeded and
// config.Load gets to report the real reason.
func TestResolveConfigPathDirectoryIsNotSeeded(t *testing.T) {
	dir := t.TempDir()

	got, seeded := resolveConfigPath(dir, true, true)
	if got != dir || seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, false)", got, seeded, dir)
	}
	ents, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	if len(ents) != 0 {
		t.Errorf("something was written into the directory: %v", ents)
	}
}

// Unreadable is not missing. A permission error means the file may well be
// there, and writing over it is the last thing we want.
func TestResolveConfigPathUnreadableIsNotSeeded(t *testing.T) {
	if os.Geteuid() == 0 {
		t.Skip("root reads through 0000 directories")
	}
	dir := filepath.Join(t.TempDir(), "locked")
	if err := os.Mkdir(dir, 0700); err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(dir, "default.toml")
	if err := os.WriteFile(target, []byte("# precious\n"), 0644); err != nil {
		t.Fatal(err)
	}
	if err := os.Chmod(dir, 0000); err != nil {
		t.Skipf("cannot drop directory permissions here: %v", err)
	}
	t.Cleanup(func() { os.Chmod(dir, 0700) })

	got, seeded := resolveConfigPath(target, true, true)
	if got != target || seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, false)", got, seeded, target)
	}
}

// An agent that took its identity from the environment runs without a config
// file at all. Seeding one would put a full template back into its volume, and
// the operator would be right to read that template as the node's settings --
// when in fact the node ignores every line of it and follows its front.
func TestResolveConfigPathDoesNotSeedWhenSeedingIsOff(t *testing.T) {
	target := filepath.Join(t.TempDir(), "default.toml")

	got, seeded := resolveConfigPath(target, true, false)
	if got != target || seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, false)", got, seeded, target)
	}
	if _, err := os.Stat(target); !os.IsNotExist(err) {
		t.Fatalf("a config file was written for a file-less agent: %v", err)
	}
}

// A path whose parent is a file fails with something that is not "missing"
// either, and it is the one such case CI can exercise as root.
func TestResolveConfigPathUnstattableIsNotSeeded(t *testing.T) {
	notADir := filepath.Join(t.TempDir(), "file")
	if err := os.WriteFile(notADir, []byte("not a directory\n"), 0644); err != nil {
		t.Fatal(err)
	}
	target := filepath.Join(notADir, "default.toml")

	got, seeded := resolveConfigPath(target, true, true)
	if got != target || seeded {
		t.Fatalf("resolveConfigPath = (%q, %v), want (%q, false)", got, seeded, target)
	}
	b, err := os.ReadFile(notADir)
	if err != nil || string(b) != "not a directory\n" {
		t.Errorf("the file in the way was touched: %q, %v", b, err)
	}
}

// The agent and the front end of one pod start together on the same empty
// volume. Whoever loses the race must still read a complete config, never a
// half-written one.
func TestWriteDefaultConfigConcurrentSeedIsAtomic(t *testing.T) {
	dir := t.TempDir()
	target := filepath.Join(dir, "default.toml")

	const racers = 8
	var wg sync.WaitGroup
	errs := make([]error, racers)
	start := make(chan struct{})
	for i := range racers {
		wg.Add(1)
		go func() {
			defer wg.Done()
			<-start
			errs[i] = writeDefaultConfig(target, dir)
		}()
	}
	close(start)
	wg.Wait()

	for i, err := range errs {
		if err != nil && !errors.Is(err, os.ErrExist) {
			t.Fatalf("racer %d: %v", i, err)
		}
	}
	b, err := os.ReadFile(target)
	if err != nil {
		t.Fatalf("no config survived the race: %v", err)
	}
	// Complete, not truncated: same length as the template modulo the
	// data_dir line we rewrite.
	if len(b) < len(hydraroot.DefaultConfigTOML)/2 {
		t.Fatalf("the config looks truncated: %d bytes for a %d byte template", len(b), len(hydraroot.DefaultConfigTOML))
	}
	if !strings.Contains(string(b), "[daemon]") {
		t.Errorf("the config is not a config:\n%s", b)
	}
	// No temporary file left behind.
	ents, err := os.ReadDir(dir)
	if err != nil {
		t.Fatal(err)
	}
	for _, e := range ents {
		if e.Name() != "default.toml" {
			t.Errorf("leftover in the config directory: %s", e.Name())
		}
	}
}
