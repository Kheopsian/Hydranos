// Package fsinfo answers one question the rest of Hydra keeps needing: is this
// path on a network filesystem? Two things Hydra puts in data_dir cannot work
// over the network — SQLite's WAL (it needs a shared-memory file the server
// cannot provide) and the engines' unix sockets (a kernel object, not a file
// a share can host). Detecting it up front lets us degrade deliberately
// instead of dying with "database is locked" halfway through boot.
package fsinfo

import "sync"

// Kind names the detected filesystem family for humans. Empty when the path is
// local (or when we could not tell, which we treat as local: a false "network"
// would push a perfectly good local install into the slower degraded mode).
type Kind string

// IsNetwork reports whether path lives on a network filesystem, and which one.
// A path that does not exist yet is resolved against its nearest existing
// parent, so this can be called before data_dir has been created.
//
// Results are memoised: boot asks this for the store, for each engine socket
// and for the banner, and the answer cannot change under a running process
// without a remount — which we do not survive anyway.
func IsNetwork(path string) (bool, Kind) {
	if v, ok := cache.Load(path); ok {
		k := v.(Kind)
		return k != "", k
	}
	k := detect(path)
	cache.Store(path, k)
	return k != "", k
}

var cache sync.Map
