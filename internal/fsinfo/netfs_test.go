package fsinfo

import (
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
