// Package sqlitex builds SQLite connection strings that match what the
// underlying filesystem can actually do. Every SQLite file Hydra opens lives in
// data_dir, so every one of them faces the same question when data_dir is on a
// share — keeping the answer in one place stops the next database from
// rediscovering it the hard way.
package sqlitex

import "github.com/Kheopsian/hydra/internal/fsinfo"

// DSN returns the connection string for a SQLite file plus any caller-specific
// pragmas, and reports whether the network fallback was taken. When it was, the
// caller MUST cap the pool at one connection: the fallback relies on a single
// exclusive owner.
//
// On a network filesystem WAL is impossible (it needs a shared-memory index
// file no SMB/NFS server can back) and, measured on a real CIFS mount with
// nounix,soft, journal_mode=DELETE + locking_mode=EXCLUSIVE is still not
// enough: SQLite takes a POSIX lock the share refuses before the locking mode
// applies, and every write fails SQLITE_BUSY. nolock=1 is what actually makes
// it work. That is only safe because EXCLUSIVE keeps one connection and Hydra
// owns these files alone — two processes over one network database would
// corrupt it.
func DSN(path, extra string) (dsn string, networkFS bool) {
	if net, _ := fsinfo.IsNetwork(path); net {
		return NetworkDSN(path, extra), true
	}
	return "file:" + path +
		"?_pragma=journal_mode(WAL)" +
		"&_pragma=synchronous(NORMAL)" +
		"&_pragma=busy_timeout(5000)" + extra, false
}

// NetworkDSN is the share connection string on its own, so code that has to
// reason about that mode without living on a share can ask for it by name. The
// repair path needs exactly this: it must prove a converted database opens the
// way a share will open it, and asking DSN would hand it the local WAL string
// and undo the conversion it just made.
//
// synchronous(FULL): with no WAL there is no log to replay, and the link itself
// can drop mid-write, so durability is bought per commit.
func NetworkDSN(path, extra string) string {
	return "file:" + path +
		"?nolock=1" +
		"&_pragma=journal_mode(DELETE)" +
		"&_pragma=locking_mode(EXCLUSIVE)" +
		"&_pragma=synchronous(FULL)" +
		"&_pragma=busy_timeout(15000)" + extra
}
