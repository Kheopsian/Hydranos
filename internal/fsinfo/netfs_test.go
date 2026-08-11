package fsinfo

import (
	"os"
	"path/filepath"
	"testing"
)

// A temp dir is local by definition. The regression this guards against is the
// probe or the magic table demoting a perfectly good local filesystem, which
// would silently push every install into the slow, less durable store mode.
func TestLocalDirIsNotNetwork(t *testing.T) {
	if net, kind := IsNetwork(t.TempDir()); net {
		t.Fatalf("temp dir reported as network storage (%q)", kind)
	}
}

// data_dir usually does not exist yet on first run: the answer must come from
// the nearest existing parent rather than a stat failure defaulting to "local"
// for the wrong reason.
func TestNonExistentPathResolvesToParent(t *testing.T) {
	deep := filepath.Join(t.TempDir(), "not", "created", "yet")
	if net, kind := IsNetwork(deep); net {
		t.Fatalf("unborn path under a local dir reported as network (%q)", kind)
	}
}

// store.Open asks about data_dir/hydra.db, not about data_dir. On an existing
// install that file resolves to itself, and joining a socket name onto a file
// gives ENOTDIR -- which read as "the share refused the socket" and demoted a
// perfectly local install to the network fallback. That fallback's DSN carries
// nolock=1, which cannot open a WAL database, so an install that already had
// one failed to open its store on every boot: no categories, no counters, and
// the legacy JSON sidecars rewritten from an empty in-memory state. A fresh
// install has no database file yet and walks up to the directory, which is why
// this only ever hit upgrades. A file must answer what its directory answers.
func TestExistingFileAnswersLikeItsDirectory(t *testing.T) {
	dir := t.TempDir()
	file := filepath.Join(dir, "hydra.db")
	if err := os.WriteFile(file, []byte("x"), 0o644); err != nil {
		t.Fatal(err)
	}
	dirNet, dirKind := IsNetwork(dir)
	fileNet, fileKind := IsNetwork(file)
	if fileNet != dirNet || fileKind != dirKind {
		t.Fatalf("file reported (%v,%q) but its directory reported (%v,%q)",
			fileNet, fileKind, dirNet, dirKind)
	}
}

// The second call must agree with the first: a memoised answer that disagreed
// would give the store one journal mode and the socket path another.
func TestResultIsStable(t *testing.T) {
	dir := t.TempDir()
	first, k1 := IsNetwork(dir)
	second, k2 := IsNetwork(dir)
	if first != second || k1 != k2 {
		t.Fatalf("unstable answer: (%v,%q) then (%v,%q)", first, k1, second, k2)
	}
}
