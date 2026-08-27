//go:build !windows

package engine

import (
	"net"
	"os"
	"os/exec"
	"strconv"
	"strings"
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

// bindToDeviceSupported reports whether a socket can be pinned to an interface
// by NAME on this platform. Linux can (SO_BINDTODEVICE); Windows cannot, and
// falls back to pinning the source address.
const bindToDeviceSupported = true

// applyEgressControl pins the dialer's sockets to an interface by name and
// marks them, in ONE Control hook: net.Dialer has a single Control, so the two
// have to be composed rather than assigned one after the other -- which is how
// a second setting would silently drop the first.
//
// Binding by name is what actually steers the egress; see pinDialerToInterface
// for the measurement that made this necessary.
func applyEgressControl(d *net.Dialer, iface string, fwmark int) {
	iface = strings.TrimSpace(iface)
	if iface == "" && fwmark == 0 {
		return
	}
	d.Control = func(network, address string, c syscall.RawConn) error {
		var sockErr error
		if err := c.Control(func(fd uintptr) {
			if iface != "" {
				sockErr = syscall.SetsockoptString(int(fd), syscall.SOL_SOCKET, syscall.SO_BINDTODEVICE, iface)
				if sockErr != nil {
					return
				}
			}
			if fwmark != 0 {
				sockErr = syscall.SetsockoptInt(int(fd), syscall.SOL_SOCKET, soMark, fwmark)
			}
		}); err != nil {
			return err
		}
		return sockErr
	}
}

// applyFwmark makes the dialer tag outbound packets with SO_MARK so policy
// routing can steer per-tunnel (multi-tunnel Proton). No-op when fwmark==0.
func applyFwmark(d *net.Dialer, fwmark int) { applyEgressControl(d, "", fwmark) }

// signalHeapDump asks the engine's jemalloc (SIGUSR1 handler in main.rs) to
// dump a heap profile before the watchdog kills it.
func signalHeapDump(pid int) { _ = syscall.Kill(pid, syscall.SIGUSR1) }

// selfSIGTERM triggers the graceful SIGTERM shutdown path (saveState + flush).
func selfSIGTERM() { _ = syscall.Kill(os.Getpid(), syscall.SIGTERM) }

// procStat reads /proc/<pid>/stat and returns (alive, rssBytes). A process in
// state Z (zombie) or X (dead) counts as NOT alive. Reads the RSS field
// (resident set size in pages, overall field 24) and converts to bytes.
func procStat(pid int) (bool, int64) {
	data, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return false, 0 // process gone
	}
	s := string(data)
	// The comm field (field 2) is parenthesised and can itself contain spaces
	// and parens, so anchor on the LAST ')': state is the token right after it.
	rp := strings.LastIndexByte(s, ')')
	if rp < 0 || rp+2 >= len(s) {
		return false, 0
	}
	// rest[0] == field 3 (state); overall field N maps to rest[N-3].
	rest := strings.Fields(s[rp+2:])
	if len(rest) < 1 {
		return false, 0
	}
	if state := rest[0]; state == "Z" || state == "X" || state == "x" {
		return false, 0
	}
	// rss is overall field 24 -> rest[21].
	if len(rest) > 21 {
		if pages, err := strconv.ParseInt(rest[21], 10, 64); err == nil {
			return true, pages * int64(os.Getpagesize())
		}
	}
	return true, 0
}

// hideChildWindow is a no-op off Windows: there are no console windows to hide.
func hideChildWindow(cmd *exec.Cmd) {}
