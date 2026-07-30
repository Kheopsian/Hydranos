//go:build windows

package engine

import (
	"net"
	"os"
	"path/filepath"
	"strings"
)

const engineBinaryName = "hydra-engine.exe"
const engineBinaryDefault = "hydra-engine.exe"

// statDev returns a volume-level id (Windows has no dev/inode). Same-volume
// paths share an id so the cross-filesystem hardlink-move guard still works at
// volume granularity; derived from the volume name, never errors.
func statDev(p string) (uint64, error) {
	vol := strings.ToUpper(filepath.VolumeName(filepath.Clean(p)))
	var h uint64 = 1469598103934665603 // FNV-1a offset basis
	for _, c := range vol {
		h ^= uint64(c)
		h *= 1099511628211
	}
	return h, nil
}

// applyFwmark is a no-op on Windows: VPN binding is done by the system VPN
// client (whole-network / per-app), not per-socket fwmark (no netfilter).
func applyFwmark(d *net.Dialer, fwmark int) {}

// signalHeapDump is a no-op on Windows (no jemalloc SIGUSR1 handler).
func signalHeapDump(pid int) {}

// selfSIGTERM exits the process; a Windows service manager restarts it.
func selfSIGTERM() { os.Exit(1) }
