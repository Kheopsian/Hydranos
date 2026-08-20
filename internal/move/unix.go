//go:build !windows

package move

import (
	"os"
	"syscall"
)

func linkCount(info os.FileInfo) uint64 {
	if st, ok := info.Sys().(*syscall.Stat_t); ok {
		return uint64(st.Nlink)
	}
	return 1
}

// sameFilesystem reports whether a rename between the two paths is possible.
//
// The target usually does not exist yet, so the check walks up to the nearest
// existing ancestor: the device a not-yet-created directory will live on is
// the device of the directory that will contain it.
func sameFilesystem(a, b string) bool {
	da, err := devOf(a)
	if err != nil {
		return false
	}
	db, err := devOfNearest(b)
	if err != nil {
		return false
	}
	return da == db
}

func devOf(p string) (uint64, error) {
	var st syscall.Stat_t
	if err := syscall.Stat(p, &st); err != nil {
		return 0, err
	}
	return uint64(st.Dev), nil
}

func devOfNearest(p string) (uint64, error) {
	for {
		if d, err := devOf(p); err == nil {
			return d, nil
		}
		parent := parentDir(p)
		if parent == p {
			return 0, os.ErrNotExist
		}
		p = parent
	}
}

func freeSpace(p string) int64 {
	target := p
	for {
		var st syscall.Statfs_t
		if err := syscall.Statfs(target, &st); err == nil {
			return int64(st.Bavail) * int64(st.Bsize)
		}
		parent := parentDir(target)
		if parent == target {
			return 0
		}
		target = parent
	}
}
