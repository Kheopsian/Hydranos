//go:build !windows

package move

import (
	"errors"
	"os"
	"syscall"
)

// isCrossDevice reports whether a failed rename failed because the two paths
// are on different mounts.
//
// This is the only reliable way to know. Comparing st_dev is not enough: two
// bind mounts of one filesystem share a device number and rename(2) still
// returns EXDEV between them, which is the normal shape of a container that
// mounts several host directories from the same pool.
func isCrossDevice(err error) bool {
	var le *os.LinkError
	if errors.As(err, &le) {
		return errors.Is(le.Err, syscall.EXDEV)
	}
	return errors.Is(err, syscall.EXDEV)
}
