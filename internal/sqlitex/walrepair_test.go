package sqlitex

import (
	"database/sql"
	"errors"
	"os"
	"path/filepath"
	"testing"

	_ "modernc.org/sqlite"
)

// makeWAL builds a small database in WAL mode, closed cleanly so its log is
// checkpointed, and returns its path.
func makeWAL(t *testing.T) string {
	t.Helper()
	path := filepath.Join(t.TempDir(), "hydra.db")
	db, err := sql.Open("sqlite", "file:"+path+"?_pragma=journal_mode(WAL)")
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec(`CREATE TABLE t(a TEXT); INSERT INTO t VALUES('kept')`); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	if wal, _ := headerSaysWAL(path); !wal {
		t.Fatal("setup produced a database that is not in WAL")
	}
	return path
}

// rowsOf reads the fixture table back through a plain read-only open, which is
// how we check a conversion moved the header without touching the content.
func rowsOf(t *testing.T, path string) []string {
	t.Helper()
	db, err := sql.Open("sqlite", "file:"+path+"?mode=ro")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	rows, err := db.Query(`SELECT a FROM t`)
	if err != nil {
		t.Fatal(err)
	}
	defer rows.Close()
	var out []string
	for rows.Next() {
		var s string
		if err := rows.Scan(&s); err != nil {
			t.Fatal(err)
		}
		out = append(out, s)
	}
	return out
}

// The premise of the whole repair: nolock cannot open a WAL database, even a
// cleanly closed one with no -wal left. If this ever stops being true the
// repair has no reason to exist.
func TestNolockCannotOpenWAL(t *testing.T) {
	path := makeWAL(t)
	db, err := sql.Open("sqlite", "file:"+path+"?nolock=1")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if err := db.Ping(); err == nil {
		t.Fatal("nolock opened a WAL database; the fallback would not need repairing")
	}
}

// A local database is never a repair case, however it is journalled: the
// fallback that cannot open it is not in play.
func TestLocalWALIsNotARepairCase(t *testing.T) {
	d, err := Diagnose(makeWAL(t))
	if err != nil {
		t.Fatal(err)
	}
	if !d.InWAL {
		t.Fatal("a WAL database did not report InWAL")
	}
	if d.NeedsRepair() {
		t.Fatal("a local WAL database asked to be repaired")
	}
}

// An absent database is the normal first-run case and must not look broken.
func TestAbsentDatabaseIsNotARepairCase(t *testing.T) {
	d, err := Diagnose(filepath.Join(t.TempDir(), "not-created-yet.db"))
	if err != nil {
		t.Fatalf("diagnosing an absent database failed: %v", err)
	}
	if d.InWAL || d.NeedsRepair() {
		t.Fatalf("absent database reported %+v", d)
	}
}

// The route we prefer: SQLite converts, the content survives, and the file now
// opens the way the daemon will open it on a share.
func TestConvertKeepsTheDataAndOpensUnderNolock(t *testing.T) {
	path := makeWAL(t)
	method, err := Convert(path)
	if err != nil {
		t.Fatalf("convert failed: %v", err)
	}
	if method != "pragma" {
		t.Fatalf("expected the checkpointing route on a local disk, got %q", method)
	}
	if wal, _ := headerSaysWAL(path); wal {
		t.Fatal("header still says WAL after conversion")
	}
	if got := rowsOf(t, path); len(got) != 1 || got[0] != "kept" {
		t.Fatalf("content changed: %v", got)
	}
	db, err := sql.Open("sqlite", "file:"+path+"?nolock=1")
	if err != nil {
		t.Fatal(err)
	}
	defer db.Close()
	if err := db.Ping(); err != nil {
		t.Fatalf("nolock still cannot open the converted database: %v", err)
	}
}

// The fallback route, exercised directly because a filesystem that refuses the
// checkpoint cannot be conjured in a unit test. Two bytes change, the rows do
// not, and the stale sidecars are gone.
func TestHeaderRouteConvertsWithoutTouchingContent(t *testing.T) {
	path := makeWAL(t)
	before := rowsOf(t, path)

	if err := convertViaHeader(path); err != nil {
		t.Fatalf("header conversion failed: %v", err)
	}
	if wal, _ := headerSaysWAL(path); wal {
		t.Fatal("header still says WAL")
	}
	if after := rowsOf(t, path); len(after) != len(before) || after[0] != before[0] {
		t.Fatalf("content changed: %v -> %v", before, after)
	}
	if _, err := os.Stat(path + "-shm"); !os.IsNotExist(err) {
		t.Fatal("a stale -shm survived the conversion")
	}
}

// The one case that could destroy data: a -wal holding frames the database does
// not have yet. The header must not be touched, and the file must be left
// exactly as it was.
func TestHeaderRouteRefusesAHotLog(t *testing.T) {
	path := makeWAL(t)
	if err := os.WriteFile(path+"-wal", []byte("frames pending, not yet merged"), 0o644); err != nil {
		t.Fatal(err)
	}

	err := convertViaHeader(path)
	if !errors.Is(err, ErrHotWAL) {
		t.Fatalf("expected ErrHotWAL, got %v", err)
	}
	if wal, _ := headerSaysWAL(path); !wal {
		t.Fatal("the header was rewritten despite the hot log")
	}
	if _, serr := os.Stat(path + "-wal"); serr != nil {
		t.Fatal("the pending log was removed")
	}
}

// Diagnose has to notice a hot log, since that is what decides whether the
// header route stays available.
func TestDiagnoseReportsAHotLog(t *testing.T) {
	path := makeWAL(t)
	if err := os.WriteFile(path+"-wal", []byte("pending"), 0o644); err != nil {
		t.Fatal(err)
	}
	d, err := Diagnose(path)
	if err != nil {
		t.Fatal(err)
	}
	if !d.HotWAL {
		t.Fatal("a non-empty -wal was not reported as hot")
	}
}

// The backup is the user's safety net, so it must be a real copy and must not
// disturb the original.
func TestBackupCopiesTheDatabase(t *testing.T) {
	path := makeWAL(t)
	dst, err := Backup(path)
	if err != nil {
		t.Fatalf("backup failed: %v", err)
	}
	orig, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}
	copied, err := os.ReadFile(dst)
	if err != nil {
		t.Fatal(err)
	}
	if len(orig) != len(copied) {
		t.Fatalf("backup is %d bytes, original is %d", len(copied), len(orig))
	}
	for i := range orig {
		if orig[i] != copied[i] {
			t.Fatalf("backup differs from the original at byte %d", i)
		}
	}
	if got := rowsOf(t, dst); len(got) != 1 || got[0] != "kept" {
		t.Fatalf("the backup is not a usable database: %v", got)
	}
}

// Converting something that is not a database must fail loudly rather than
// stamp two bytes into an unrelated file.
func TestConvertRefusesANonDatabase(t *testing.T) {
	path := filepath.Join(t.TempDir(), "notadb.db")
	if err := os.WriteFile(path, []byte("this is not a SQLite file at all, not even close"), 0o644); err != nil {
		t.Fatal(err)
	}
	if _, err := Convert(path); err == nil {
		t.Fatal("converting a non-database succeeded")
	}
}

// A database already using the rollback journal is not an error to convert: the
// button has to be safe to press twice.
func TestConvertIsIdempotent(t *testing.T) {
	path := makeWAL(t)
	if _, err := Convert(path); err != nil {
		t.Fatal(err)
	}
	method, err := Convert(path)
	if err != nil {
		t.Fatalf("second conversion failed: %v", err)
	}
	if method != "already-rollback" {
		t.Fatalf("expected already-rollback on the second run, got %q", method)
	}
}
