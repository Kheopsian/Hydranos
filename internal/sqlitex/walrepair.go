package sqlitex

import (
	"database/sql"
	"errors"
	"fmt"
	"io"
	"os"
	"time"

	"github.com/Kheopsian/hydra/internal/fsinfo"
	_ "modernc.org/sqlite"
)

// Repairing a WAL database that has landed on a share.
//
// The network fallback opens with nolock=1, and SQLite refuses nolock on a
// database whose header says WAL: the WAL needs a -shm index that nolock
// disables, so the open fails with SQLITE_CANTOPEN before a single statement
// runs. That is documented behaviour, not a bug in nolock -- but it is reached
// by an ordinary move. An install that ran locally has a WAL database, and
// pointing data_dir at a share leaves it unopenable, permanently, with no way
// to heal itself and nothing on screen but a failure to start.
//
// Converting it is a two-byte change to the file header, and the honest way to
// make it is to let SQLite do it (PRAGMA journal_mode=DELETE, which also
// checkpoints anything still in the -wal). We only write those bytes ourselves
// when the share refuses that write, and then only once we are certain there is
// no unmerged content to lose.

// Header offsets 18 and 19 are the write and read format versions: 1 for the
// rollback journal, 2 for WAL. They are the whole difference between a file
// nolock can open and one it cannot.
const (
	hdrWriteVersion = 18
	hdrReadVersion  = 19
	fmtLegacy       = 1
	fmtWAL          = 2
	hdrLen          = 100
	magic           = "SQLite format 3"
)

// ErrHotWAL says the -wal still holds frames that are not in the database yet.
// Rewriting the header at that point would silently drop committed
// transactions, so we refuse: the file has to be checkpointed somewhere that
// can lock it, which in practice means the machine it came from.
var ErrHotWAL = errors.New("the write-ahead log still holds unmerged changes")

// Diagnosis describes why a database cannot be opened where it now lives.
type Diagnosis struct {
	Path string
	// OnNetwork is true when the path resolves to a filesystem that forces the
	// nolock fallback.
	OnNetwork bool
	// InWAL is true when the file exists and its header says WAL.
	InWAL bool
	// HotWAL is true when a -wal sidecar still has content, which makes the
	// checkpointing route the only safe one.
	HotWAL bool
}

// NeedsRepair reports whether this is the WAL-on-a-share case: the file exists,
// it is in WAL, and it lives where only nolock will do. Anything else (absent
// file, local disk, already a rollback journal) must open normally and is none
// of this code's business.
func (d Diagnosis) NeedsRepair() bool { return d.OnNetwork && d.InWAL }

// Diagnose inspects the file directly rather than through SQLite, so it stays
// answerable in exactly the situation where opening is what fails.
func Diagnose(path string) (Diagnosis, error) {
	d := Diagnosis{Path: path}
	onNet, _ := fsinfo.IsNetwork(path)
	d.OnNetwork = onNet

	inWAL, err := headerSaysWAL(path)
	if err != nil {
		if os.IsNotExist(err) {
			return d, nil // a database that does not exist yet gets created correctly
		}
		return d, err
	}
	d.InWAL = inWAL

	if st, serr := os.Stat(path + "-wal"); serr == nil && st.Size() > 0 {
		d.HotWAL = true
	}
	return d, nil
}

// headerSaysWAL reads the two format bytes. A file too short to hold a header
// is not a database anyone should be converting, and says so.
func headerSaysWAL(path string) (bool, error) {
	f, err := os.Open(path)
	if err != nil {
		return false, err
	}
	defer f.Close()

	st, err := f.Stat()
	if err != nil {
		return false, err
	}
	if st.Size() == 0 {
		// SQLite creates the file before it writes a header. Nothing has been
		// decided yet, so there is nothing to convert.
		return false, nil
	}

	var hdr [hdrLen]byte
	n, err := io.ReadFull(f, hdr[:])
	if err != nil && !errors.Is(err, io.ErrUnexpectedEOF) {
		return false, err
	}
	if n < len(magic) || string(hdr[:len(magic)]) != magic {
		return false, fmt.Errorf("not a SQLite database: %s", path)
	}
	if n < hdrLen {
		return false, fmt.Errorf("truncated SQLite header: %s", path)
	}
	return hdr[hdrWriteVersion] == fmtWAL || hdr[hdrReadVersion] == fmtWAL, nil
}

// Backup copies the database beside itself before anything rewrites it and
// returns the copy's path.
//
// The -wal and -shm are deliberately not copied: a conversion only ever runs
// against a checkpointed database, so they hold nothing the copy would need,
// and a stale -shm restored next to a file would confuse the next open. A copy
// that cannot be finished (no space, share dropped) is removed rather than
// left behind, because a truncated backup invites exactly the trust it cannot
// honour.
func Backup(path string) (string, error) {
	dst := fmt.Sprintf("%s.bak-preconvert-%s", path, time.Now().Format("20060102-150405"))

	src, err := os.Open(path)
	if err != nil {
		return "", err
	}
	defer src.Close()

	out, err := os.OpenFile(dst, os.O_WRONLY|os.O_CREATE|os.O_EXCL, 0o644)
	if err != nil {
		return "", err
	}
	if _, err := io.Copy(out, src); err != nil {
		out.Close()
		os.Remove(dst)
		return "", fmt.Errorf("copying the database: %w", err)
	}
	// Durability matters more than speed here: this copy is the only thing
	// between a failed conversion and a database nobody can rebuild.
	if err := out.Sync(); err != nil {
		out.Close()
		os.Remove(dst)
		return "", err
	}
	if err := out.Close(); err != nil {
		os.Remove(dst)
		return "", err
	}
	return dst, nil
}

// Convert takes a database out of WAL so the nolock fallback can open it, and
// reports which route did it.
//
// The route that keeps SQLite in charge is tried first, because that one also
// checkpoints. Only when the filesystem refuses that write do we set the header
// bytes directly, and only when nothing is left unmerged. Either way the result
// is verified by opening the file the way the daemon actually will.
func Convert(path string) (method string, err error) {
	d, err := Diagnose(path)
	if err != nil {
		return "", err
	}
	if !d.InWAL {
		return "already-rollback", nil // nothing to do, and that is not a failure
	}

	pragmaErr := convertViaSQLite(path)
	switch {
	case pragmaErr == nil:
		method = "pragma"
	case d.HotWAL:
		// The clean route failed and the log is hot: the two-byte rewrite would
		// drop whatever the -wal still holds. Refuse, and leave the file alone.
		return "", fmt.Errorf("%w (this filesystem also refused the checkpoint: %v)", ErrHotWAL, pragmaErr)
	default:
		if herr := convertViaHeader(path); herr != nil {
			return "", fmt.Errorf("checkpoint route failed (%v), header route failed: %w", pragmaErr, herr)
		}
		method = "header"
	}

	if err := verifyOpens(path); err != nil {
		return method, fmt.Errorf("converted but the database still does not open: %w", err)
	}
	return method, nil
}

// convertViaSQLite is the route we want: SQLite checkpoints the log, rewrites
// the header and leaves a consistent file. It needs locking, which is exactly
// what a hostile share denies.
func convertViaSQLite(path string) error {
	db, err := sql.Open("sqlite", "file:"+path+"?_pragma=busy_timeout(15000)")
	if err != nil {
		return err
	}
	defer db.Close()
	db.SetMaxOpenConns(1)

	var mode string
	if err := db.QueryRow(`PRAGMA journal_mode=DELETE`).Scan(&mode); err != nil {
		return err
	}
	if mode != "delete" {
		return fmt.Errorf("journal_mode is %q after the change, wanted delete", mode)
	}
	return nil
}

// convertViaHeader writes the two format bytes itself. Safe only against a
// checkpointed database, which is the caller's precondition and is re-checked
// here rather than assumed: this is the one path that can destroy data.
func convertViaHeader(path string) error {
	if st, err := os.Stat(path + "-wal"); err == nil && st.Size() > 0 {
		return ErrHotWAL
	}

	f, err := os.OpenFile(path, os.O_RDWR, 0)
	if err != nil {
		return err
	}
	defer f.Close()

	var hdr [hdrLen]byte
	if _, err := io.ReadFull(f, hdr[:]); err != nil {
		return err
	}
	if string(hdr[:len(magic)]) != magic {
		return fmt.Errorf("not a SQLite database: %s", path)
	}

	if _, err := f.WriteAt([]byte{fmtLegacy, fmtLegacy}, hdrWriteVersion); err != nil {
		return err
	}
	if err := f.Sync(); err != nil {
		return err
	}
	// A -shm describes a WAL index that no longer applies, and an empty -wal is
	// just as stale. Leaving either would have the next open read a map of a
	// file that is not in WAL any more.
	_ = os.Remove(path + "-shm")
	_ = os.Remove(path + "-wal")
	return nil
}

// verifyOpens proves the repair worked by opening the file exactly as the
// daemon will and reading through it. A conversion that reported success on a
// file that still does not open would be the worst outcome available: the user
// restarts into the same wall, minus the explanation.
func verifyOpens(path string) error {
	db, err := sql.Open("sqlite", NetworkDSN(path, ""))
	if err != nil {
		return err
	}
	defer db.Close()
	db.SetMaxOpenConns(1)

	var n int
	if err := db.QueryRow(`SELECT count(*) FROM sqlite_schema`).Scan(&n); err != nil {
		return err
	}
	var res string
	if err := db.QueryRow(`PRAGMA integrity_check`).Scan(&res); err != nil {
		return err
	}
	if res != "ok" {
		return fmt.Errorf("integrity check says %q", res)
	}
	return nil
}
