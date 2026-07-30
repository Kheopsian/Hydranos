//go:build !windows

package api

import (
	"os"
	"syscall"
)

// selfTerminate triggers the graceful SIGTERM shutdown path (saveState + engine
// flush); the supervisor (Docker --restart) then reboots a clean pair.
func selfTerminate() { _ = syscall.Kill(os.Getpid(), syscall.SIGTERM) }
