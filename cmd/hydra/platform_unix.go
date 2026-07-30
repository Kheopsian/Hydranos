//go:build !windows

package main

import (
	"log/slog"
	"syscall"
)

// defaultEngineTCP selects the TCP loopback IPC by default on platforms
// without Unix domain sockets in this path (Windows). Linux defaults to unix.
const defaultEngineTCP = false

// raiseNofileLimit bumps the process soft RLIMIT_NOFILE toward target so the
// engines can hold many peer sockets. Best-effort; logs and returns on failure.
func raiseNofileLimit(target uint64) {
	var rlim syscall.Rlimit
	if err := syscall.Getrlimit(syscall.RLIMIT_NOFILE, &rlim); err != nil {
		slog.Error("getrlimit failed", "error", err)
		return
	}
	slog.Info("nofile limit", "current_soft", rlim.Cur, "current_hard", rlim.Max, "target", target)
	if rlim.Cur >= target {
		return
	}
	rlim.Cur = target
	if rlim.Max < target {
		rlim.Max = target
	}
	if err := syscall.Setrlimit(syscall.RLIMIT_NOFILE, &rlim); err != nil {
		slog.Error("setrlimit failed", "error", err, "target", target)
	} else {
		var after syscall.Rlimit
		_ = syscall.Getrlimit(syscall.RLIMIT_NOFILE, &after)
		slog.Info("nofile raised", "soft", after.Cur, "hard", after.Max)
	}
}
