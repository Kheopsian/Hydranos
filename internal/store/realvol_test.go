package store

import (
	"os"
	"testing"
	"time"
)

// TestImportRealVolume runs only when STATE_PATH is set. It imports the real
// prod state.json into a throwaway DB and reports counts, timing and DB size.
// Run inside a container that mounts appdata so container-paths resolve, e.g.:
//
//	STATE_PATH=/configs/state.json DB_OUT=/tmp/dry.db go test -run RealVolume -v
func TestImportRealVolume(t *testing.T) {
	sp := os.Getenv("STATE_PATH")
	if sp == "" {
		t.Skip("STATE_PATH not set")
	}
	dbPath := os.Getenv("DB_OUT")
	if dbPath == "" {
		dbPath = t.TempDir() + "/dry.db"
	}
	os.Remove(dbPath)

	s, err := Open(dbPath)
	if err != nil {
		t.Fatal(err)
	}
	defer s.Close()

	start := time.Now()
	res, err := s.ImportLegacy(sp)
	if err != nil {
		t.Fatalf("import: %v", err)
	}
	elapsed := time.Since(start)

	h, _ := s.Count(Hoard)
	r, _ := s.Count(Race)
	var dbSize int64
	if fi, err := os.Stat(dbPath); err == nil {
		dbSize = fi.Size()
	}
	t.Logf("imported=%d missing_file=%d errors=%d | hoard=%d race=%d | %.1fs | db=%.1f MB",
		res.Imported, res.MissingFile, res.Errors, h, r,
		elapsed.Seconds(), float64(dbSize)/1e6)
}
