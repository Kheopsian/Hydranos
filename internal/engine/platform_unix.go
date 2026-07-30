//go:build !windows

package engine

import (
	"net"
	"os"
	"syscall"
)

// soMark is SO_MARK (Linux). Value from include/asm-generic/socket.h.
const soMark = 36

const engineBinaryName = "hydra-engine"
const engineBinaryDefault = "/usr/local/bin/hydra-engine"

// statDev returns the device id of the filesystem holding p (for the
// cross-filesystem hardlink-move guard).
func statDev(p string) (uint64, error) {
	var st syscall.Stat_t
	if err := syscall.Stat(p, &st); err != nil {
		return 0, err
	}
	return uint64(st.Dev), nil
}

// applyFwmark makes the dialer tag outbound packets with SO_MARK so policy
// routing can steer per-tunnel (multi-tunnel Proton). No-op when fwmark==0.
func applyFwmark(d *net.Dialer, fwmark int) {
	if fwmark == 0 {
		return
	}
	d.Control = func(network, address string, c syscall.RawConn) error {
		var sockErr error
		if err := c.Control(func(fd uintptr) {
			sockErr = syscall.SetsockoptInt(int(fd), syscall.SOL_SOCKET, soMark, fwmark)
		}); err != nil {
			return err
		}
		return sockErr
	}
}

// signalHeapDump asks the engine's jemalloc (SIGUSR1 handler in main.rs) to
// dump a heap profile before the watchdog kills it.
func signalHeapDump(pid int) { _ = syscall.Kill(pid, syscall.SIGUSR1) }

// selfSIGTERM triggers the graceful SIGTERM shutdown path (saveState + flush).
func selfSIGTERM() { _ = syscall.Kill(os.Getpid(), syscall.SIGTERM) }
