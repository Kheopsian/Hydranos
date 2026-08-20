//go:build windows

package move

import (
	"errors"
	"os"
	"syscall"
)

// isCrossDevice reports whether a failed rename failed for want of being on
// one volume. Windows answers ERROR_NOT_SAME_DEVICE (17).
func isCrossDevice(err error) bool {
	const errorNotSameDevice = syscall.Errno(17)
	var le *os.LinkError
	if errors.As(err, &le) {
		return errors.Is(le.Err, errorNotSameDevice)
	}
	return errors.Is(err, errorNotSameDevice)
}
